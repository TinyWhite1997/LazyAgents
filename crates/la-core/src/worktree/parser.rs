//! Unified-diff parser for the diff review surface (M2.5 / WEK-28).
//!
//! Reads the raw stdout bytes of `git diff --no-color --no-ext-diff` and
//! produces an in-memory [`ParsedDiff`] with:
//!
//! - per-file headers (rename / copy / mode-change detection),
//! - per-hunk header lines (`@@ -a,b +c,d @@`),
//! - per-line origin tags (` `/`+`/`-`/`\`),
//! - a **byte-range** into the original buffer for every hunk body, so
//!   stage / unstage can re-slice the patch text instead of re-emitting
//!   it. The brief calls this out as the key fragility-reducer (see
//!   risk §5.1).
//!
//! Out of scope here: combined diffs (`--cc`) — they only appear for
//! merge previews, which M2 does not surface. The parser bails on the
//! `@@@ … @@@` header rather than guessing.

use sha2::{Digest, Sha256};

/// One parsed file inside a unified-diff stream.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    /// Path on the "+" side. For renames this is the new name.
    pub new_path: String,
    /// Path on the "-" side. Differs from `new_path` only on
    /// rename/copy.
    pub old_path: Option<String>,
    /// `true` if the file is `Binary files differ` per git's own line.
    pub is_binary: bool,
    /// Hunks for this file. Always empty when `is_binary`.
    pub hunks: Vec<ParsedHunk>,
    /// Raw byte range in the source buffer that covers this file's
    /// `diff --git` header + extended headers. Used by the patch
    /// synthesiser to splice the right `--- a/... +++ b/...` lines back
    /// in front of selected hunks.
    pub header_range: (usize, usize),
    /// Detected mode change `(old_mode, new_mode)`, if any.
    pub mode_change: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct ParsedHunk {
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<ParsedLine>,
    /// Byte range in the source buffer covering the hunk header line +
    /// every hunk line (including trailing `\n`s and any
    /// `\ No newline at end of file` markers). Slicing
    /// `buf[hunk.body_range.0..hunk.body_range.1]` yields a fragment
    /// that can be concatenated with the file header to form a valid
    /// single-hunk patch — no re-stringification required.
    pub body_range: (usize, usize),
}

#[derive(Debug, Clone, Copy)]
pub enum LineOrigin {
    Context,
    Add,
    Delete,
}

#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub origin: LineOrigin,
    pub content: String,
    pub no_newline: bool,
}

/// Top-level parse result. `Vec<ParsedFile>` because a single
/// `git diff` invocation can return many files when called without a
/// pathspec (we always call it with one, but the parser is broader for
/// future use and unit testing).
#[derive(Debug, Clone, Default)]
pub struct ParsedDiff {
    pub files: Vec<ParsedFile>,
}

/// Parse a `git diff --no-color --no-ext-diff` byte stream.
///
/// The parser is byte-position aware: every `ParsedFile.header_range`
/// and `ParsedHunk.body_range` indexes into `buf` so a downstream patch
/// synthesiser can re-slice the original bytes (per brief risk §5.1).
pub fn parse(buf: &[u8]) -> ParsedDiff {
    let mut files: Vec<ParsedFile> = Vec::new();
    let mut current_file: Option<ParsedFile> = None;
    let mut current_hunk: Option<(ParsedHunk, usize)> = None; // (hunk, body_start_byte)

    let mut cursor: usize = 0;
    while cursor < buf.len() {
        let line_end = match memchr_newline(buf, cursor) {
            Some(end) => end,
            None => buf.len(),
        };
        let line_bytes = &buf[cursor..line_end];
        let next_cursor = if line_end < buf.len() {
            line_end + 1
        } else {
            line_end
        };
        // String slice; lossy because user content may carry non-UTF-8
        // bytes inside text files. The original bytes are preserved for
        // patch re-slicing — this `line_str` is only used to interpret
        // structural markers (`diff --git`, `@@`, …).
        let line_str = std::str::from_utf8(line_bytes).unwrap_or("");

        if line_str.starts_with("diff --git ") {
            // Flush any in-progress hunk into the current file.
            if let (Some(file), Some((mut hunk, body_start))) =
                (current_file.as_mut(), current_hunk.take())
            {
                hunk.body_range = (body_start, cursor);
                file.hunks.push(hunk);
            }
            // Flush current file.
            if let Some(file) = current_file.take() {
                files.push(file);
            }
            let (old_path, new_path) = parse_diff_header(line_str);
            current_file = Some(ParsedFile {
                new_path,
                old_path: if let Some(op) = &old_path {
                    if Some(op.as_str()) == current_file.as_ref().map(|_| op.as_str()) {
                        None
                    } else {
                        Some(op.clone())
                    }
                } else {
                    None
                },
                is_binary: false,
                hunks: Vec::new(),
                header_range: (cursor, cursor),
                mode_change: None,
            });
            if let Some(f) = current_file.as_mut() {
                f.old_path = old_path;
            }
            cursor = next_cursor;
            continue;
        }

        if let Some(file) = current_file.as_mut() {
            // Track extended header lines into the file's header_range
            // until we hit either a hunk header (`@@`) or a binary
            // marker. Anything until that point is part of the patch
            // preamble — `--- a/...`, `+++ b/...`, `old mode 100644`,
            // `new mode 100755`, `rename from ...`, etc.
            if current_hunk.is_none() && !line_str.starts_with("@@") {
                file.header_range.1 = next_cursor;
                if let Some(rest) = line_str.strip_prefix("rename from ") {
                    file.old_path = Some(rest.to_string());
                }
                if let Some(rest) = line_str.strip_prefix("rename to ") {
                    file.new_path = rest.to_string();
                }
                if let Some(rest) = line_str.strip_prefix("copy from ") {
                    file.old_path = Some(rest.to_string());
                }
                if let Some(rest) = line_str.strip_prefix("copy to ") {
                    file.new_path = rest.to_string();
                }
                if let Some(rest) = line_str.strip_prefix("old mode ") {
                    if let Ok(m) = u32::from_str_radix(rest.trim(), 8) {
                        file.mode_change = Some((m, file.mode_change.map(|c| c.1).unwrap_or(0)));
                    }
                }
                if let Some(rest) = line_str.strip_prefix("new mode ") {
                    if let Ok(m) = u32::from_str_radix(rest.trim(), 8) {
                        file.mode_change = Some((file.mode_change.map(|c| c.0).unwrap_or(0), m));
                    }
                }
                if line_str.starts_with("Binary files ") && line_str.contains(" differ") {
                    file.is_binary = true;
                }
                cursor = next_cursor;
                continue;
            }
        }

        if line_str.starts_with("@@") && !line_str.starts_with("@@@") {
            // Finish previous hunk if any.
            if let (Some(file), Some((mut hunk, body_start))) =
                (current_file.as_mut(), current_hunk.take())
            {
                hunk.body_range = (body_start, cursor);
                file.hunks.push(hunk);
            }
            if let Some(file) = current_file.as_mut() {
                file.header_range.1 = cursor; // header ends just before this hunk
                if let Some(parsed) = parse_hunk_header(line_str) {
                    let body_start = cursor; // include the `@@` line itself in the body
                    current_hunk = Some((
                        ParsedHunk {
                            header: parsed.header_text,
                            old_start: parsed.old_start,
                            old_count: parsed.old_count,
                            new_start: parsed.new_start,
                            new_count: parsed.new_count,
                            lines: Vec::new(),
                            body_range: (body_start, body_start),
                        },
                        body_start,
                    ));
                }
            }
            cursor = next_cursor;
            continue;
        }

        if let Some((hunk, _)) = current_hunk.as_mut() {
            if let Some(first) = line_bytes.first().copied() {
                match first {
                    b' ' => hunk.lines.push(ParsedLine {
                        origin: LineOrigin::Context,
                        content: String::from_utf8_lossy(&line_bytes[1..]).into_owned(),
                        no_newline: false,
                    }),
                    b'+' => hunk.lines.push(ParsedLine {
                        origin: LineOrigin::Add,
                        content: String::from_utf8_lossy(&line_bytes[1..]).into_owned(),
                        no_newline: false,
                    }),
                    b'-' => hunk.lines.push(ParsedLine {
                        origin: LineOrigin::Delete,
                        content: String::from_utf8_lossy(&line_bytes[1..]).into_owned(),
                        no_newline: false,
                    }),
                    b'\\' => {
                        if let Some(last) = hunk.lines.last_mut() {
                            last.no_newline = true;
                        }
                    }
                    _ => {} // unrecognised — preserve the byte range but skip the structure
                }
            }
            cursor = next_cursor;
            continue;
        }

        cursor = next_cursor;
    }

    // Flush trailing hunk + file.
    if let (Some(file), Some((mut hunk, body_start))) =
        (current_file.as_mut(), current_hunk.take())
    {
        hunk.body_range = (body_start, cursor);
        file.hunks.push(hunk);
    }
    if let Some(file) = current_file.take() {
        files.push(file);
    }

    ParsedDiff { files }
}

/// Compute the 16-char hex hunk fingerprint described in the WEK-8
/// brief §3.3:
///
/// ```text
/// hunk_id = sha256(path | "\0" | old_start | "\0" | old_count
///                  | "\0" | sha256(body_bytes))[..16]   // 16 chars hex
/// ```
///
/// `body_bytes` is the raw byte range of the hunk including its
/// `@@` header line, every line with origin marker, and any `\ No
/// newline` marker. We deliberately do not normalise — character drift
/// inside the hunk body must invalidate the id (that is the whole point
/// of the id).
pub fn compute_hunk_id(path: &str, old_start: u32, old_count: u32, body_bytes: &[u8]) -> String {
    let inner = Sha256::digest(body_bytes);
    let mut outer = Sha256::new();
    outer.update(path.as_bytes());
    outer.update(b"\0");
    outer.update(old_start.to_string().as_bytes());
    outer.update(b"\0");
    outer.update(old_count.to_string().as_bytes());
    outer.update(b"\0");
    outer.update(inner);
    let digest = outer.finalize();
    let mut hex = String::with_capacity(16);
    for b in digest.iter().take(8) {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    hex
}

struct ParsedHunkHeader {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    header_text: String,
}

fn parse_hunk_header(line: &str) -> Option<ParsedHunkHeader> {
    // `@@ -A,B +C,D @@ optional section`
    let trimmed = line.trim_end_matches('\n');
    let body = trimmed.strip_prefix("@@")?.trim_start();
    let (range_part, _section) = body.split_once("@@").unwrap_or((body, ""));
    let mut iter = range_part.split_whitespace();
    let neg = iter.next()?.strip_prefix('-')?;
    let pos = iter.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_count(neg);
    let (new_start, new_count) = parse_count(pos);
    Some(ParsedHunkHeader {
        old_start,
        old_count,
        new_start,
        new_count,
        header_text: trimmed.to_string(),
    })
}

fn parse_count(s: &str) -> (u32, u32) {
    match s.split_once(',') {
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
        None => (s.parse().unwrap_or(0), 1),
    }
}

/// `diff --git a/path b/path` → `(Some(old), new)`. Falls back to
/// `(None, "")` on shapes git would never emit.
fn parse_diff_header(line: &str) -> (Option<String>, String) {
    let rest = match line.strip_prefix("diff --git ") {
        Some(r) => r.trim_end_matches('\n'),
        None => return (None, String::new()),
    };
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 2 {
        return (None, String::new());
    }
    let a = parts[0].strip_prefix("a/").unwrap_or(parts[0]).to_string();
    let b = parts[1].strip_prefix("b/").unwrap_or(parts[1]).to_string();
    (Some(a), b)
}

fn memchr_newline(buf: &[u8], from: usize) -> Option<usize> {
    buf[from..].iter().position(|b| *b == b'\n').map(|i| from + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_DIFF: &str = "\
diff --git a/foo.txt b/foo.txt
index 1234567..89abcde 100644
--- a/foo.txt
+++ b/foo.txt
@@ -1,3 +1,4 @@
 alpha
-beta
+BETA
+gamma
 delta
";

    #[test]
    fn parses_simple_modification() {
        let parsed = parse(SIMPLE_DIFF.as_bytes());
        assert_eq!(parsed.files.len(), 1);
        let f = &parsed.files[0];
        assert_eq!(f.new_path, "foo.txt");
        assert_eq!(f.old_path.as_deref(), Some("foo.txt"));
        assert!(!f.is_binary);
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 3);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_count, 4);
        assert_eq!(h.lines.len(), 5);
        assert!(matches!(h.lines[1].origin, LineOrigin::Delete));
        assert_eq!(h.lines[1].content, "beta");
        assert!(matches!(h.lines[2].origin, LineOrigin::Add));
        assert_eq!(h.lines[2].content, "BETA");
    }

    #[test]
    fn header_range_excludes_hunk_body() {
        let parsed = parse(SIMPLE_DIFF.as_bytes());
        let f = &parsed.files[0];
        let header_bytes = &SIMPLE_DIFF.as_bytes()[f.header_range.0..f.header_range.1];
        let header_str = std::str::from_utf8(header_bytes).unwrap();
        assert!(header_str.contains("diff --git"));
        assert!(header_str.contains("--- a/foo.txt"));
        assert!(header_str.contains("+++ b/foo.txt"));
        assert!(!header_str.contains("@@"));
    }

    #[test]
    fn body_range_covers_hunk_lines_with_header() {
        let parsed = parse(SIMPLE_DIFF.as_bytes());
        let f = &parsed.files[0];
        let body = &SIMPLE_DIFF.as_bytes()[f.hunks[0].body_range.0..f.hunks[0].body_range.1];
        let body_str = std::str::from_utf8(body).unwrap();
        assert!(body_str.starts_with("@@ -1,3 +1,4 @@"));
        assert!(body_str.contains("-beta"));
        assert!(body_str.contains("+BETA"));
    }

    #[test]
    fn no_newline_marker_attaches_to_previous_line() {
        let raw = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,1 +1,1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let parsed = parse(raw.as_bytes());
        let h = &parsed.files[0].hunks[0];
        assert_eq!(h.lines.len(), 2);
        assert!(h.lines[0].no_newline);
        assert!(h.lines[1].no_newline);
    }

    #[test]
    fn detects_rename_with_paths() {
        let raw = "\
diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
--- a/old.txt
+++ b/new.txt
@@ -1,1 +1,1 @@
-a
+b
";
        let parsed = parse(raw.as_bytes());
        let f = &parsed.files[0];
        assert_eq!(f.new_path, "new.txt");
        assert_eq!(f.old_path.as_deref(), Some("old.txt"));
    }

    #[test]
    fn detects_binary() {
        let raw = "\
diff --git a/img.png b/img.png
index 1..2 100644
Binary files a/img.png and b/img.png differ
";
        let parsed = parse(raw.as_bytes());
        assert!(parsed.files[0].is_binary);
        assert!(parsed.files[0].hunks.is_empty());
    }

    #[test]
    fn detects_mode_change() {
        let raw = "\
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
";
        let parsed = parse(raw.as_bytes());
        assert_eq!(parsed.files[0].mode_change, Some((0o100644, 0o100755)));
    }

    #[test]
    fn hunk_id_is_stable_for_same_body() {
        let body = b"@@ -1,1 +1,1 @@\n-a\n+b\n";
        let a = compute_hunk_id("foo.txt", 1, 1, body);
        let b = compute_hunk_id("foo.txt", 1, 1, body);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn hunk_id_shifts_when_body_changes() {
        let a = compute_hunk_id("foo.txt", 1, 1, b"@@ -1,1 +1,1 @@\n-a\n+b\n");
        let b = compute_hunk_id("foo.txt", 1, 1, b"@@ -1,1 +1,1 @@\n-a\n+c\n");
        assert_ne!(a, b);
    }

    #[test]
    fn hunk_id_shifts_when_path_changes() {
        let body = b"@@ -1,1 +1,1 @@\n-a\n+b\n";
        let a = compute_hunk_id("foo.txt", 1, 1, body);
        let b = compute_hunk_id("bar.txt", 1, 1, body);
        assert_ne!(a, b);
    }

    #[test]
    fn parses_multiple_files() {
        let raw = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1,1 +1,1 @@
-a
+A
diff --git a/b b/b
--- a/b
+++ b/b
@@ -1,1 +1,1 @@
-b
+B
";
        let parsed = parse(raw.as_bytes());
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].new_path, "a");
        assert_eq!(parsed.files[1].new_path, "b");
    }
}
