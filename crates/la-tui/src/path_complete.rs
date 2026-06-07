//! Filesystem directory autocompletion for the New-project modal (WEK-101).
//!
//! The modal feeds the user's raw input here and gets back:
//!
//! - the resolved absolute path (`~` expanded, env-free),
//! - the parent directory whose children seed the candidate list,
//! - the matching subdirectory **names** (hidden ones filtered, only
//!   directories — never regular files),
//! - whether the resolved path already exists as a directory.
//!
//! Kept as pure helpers (`pub fn`s plus a [`Completion`] struct) so the
//! App's modal can call them on every keystroke without dragging in any
//! tokio / blocking-aware machinery — the candidate list is small enough
//! (one `read_dir` per parent) that the cost is invisible at typing
//! speed. The helpers never panic on bad UTF-8 / permission errors; they
//! return an empty candidate list instead so the modal stays responsive.
//!
//! The "create new project" issue requires:
//!   * only directories (no files) appear as candidates,
//!   * hidden directories (`.git`, `.cache`, …) are filtered by default,
//!   * `~` at the start of input expands to the user's home directory,
//!   * non-existent paths are rejected by the App with a clear toast,
//!   * the prefix match is case-sensitive on Linux / macOS, case-
//!     insensitive on Windows — matches the OS conventions users expect.

use std::path::{Path, PathBuf};

/// Computed completion view for one raw input string.
///
/// The struct is cheap to clone and `PartialEq` so the App can keep it
/// inside [`Modal::NewProject`] without breaking the `derive(Clone,
/// PartialEq, Eq)` chain the rest of the modal machinery relies on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    /// Resolved absolute path interpretation of the input (`~` expanded,
    /// no env-var expansion). Empty when the input itself is empty.
    pub resolved: PathBuf,
    /// Directory we read children from to build [`candidates`]. Equals
    /// `resolved` when the input ends with `/` AND `resolved` is an
    /// existing directory; otherwise equals `resolved`'s parent.
    pub parent: PathBuf,
    /// Prefix the candidate names must start with. Empty when the input
    /// ends in a separator.
    pub prefix: String,
    /// Sorted, filtered subdirectory **names** (basenames only, no
    /// trailing `/`). At most [`Completion::CAP`] entries; the rest are
    /// dropped silently to keep the dropdown bounded.
    pub candidates: Vec<String>,
    /// True when [`resolved`] itself is an existing directory; used by
    /// the App to allow Enter to confirm.
    pub resolved_exists_as_dir: bool,
}

impl Completion {
    /// Hard cap on candidate list size — the dropdown is rendered as a
    /// fixed-height box and an unbounded `read_dir` on `/` would shove
    /// thousands of items into the modal. 200 is plenty for typical
    /// project dirs (`~`, `~/code`, `/home`, etc.).
    pub const CAP: usize = 200;
}

/// Expand a leading `~` in `input` to the user's home directory. Falls
/// back to leaving the string untouched when:
/// - input does not start with `~`,
/// - the OS does not expose `HOME` / `USERPROFILE`,
/// - input starts with `~user` (other users' homes are not supported —
///   this matches `bash` behaviour when `getpwnam` is unavailable, and
///   keeps the implementation `std`-only).
pub fn expand_tilde(input: &str) -> String {
    if !input.starts_with('~') {
        return input.to_string();
    }
    let Some(home) = home_dir() else {
        return input.to_string();
    };
    if input == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        let mut out = home.to_string_lossy().into_owned();
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(rest);
        return out;
    }
    // `~something` (no `/`) — unsupported, leave verbatim.
    input.to_string()
}

/// Cross-platform home directory lookup. Linux/macOS read `$HOME`;
/// Windows reads `%USERPROFILE%`. We deliberately avoid the `dirs`
/// crate to keep [`la-tui`]'s dep surface small (the workspace already
/// pulls a lot of UI deps; this one helper does not justify a new one).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Pick a sensible default starting path for the New-project modal.
/// Tries `$HOME` (or `%USERPROFILE%`); falls back to `cwd` so the modal
/// always opens with *some* path the user can edit instead of a blank.
pub fn default_starting_path() -> String {
    if let Some(h) = home_dir() {
        let mut s = h.to_string_lossy().into_owned();
        // Trailing separator means "list children of this dir" instead
        // of "match this directory by its name" — the much friendlier
        // first-keystroke experience.
        if !s.ends_with('/') {
            s.push('/');
        }
        return s;
    }
    std::env::current_dir()
        .map(|p| {
            let mut s = p.to_string_lossy().into_owned();
            if !s.ends_with('/') {
                s.push('/');
            }
            s
        })
        .unwrap_or_else(|_| "/".to_string())
}

/// Build a [`Completion`] for the given raw input string.
///
/// Reads the parent directory on every call — cheap enough for the
/// modal's keystroke cadence (one `read_dir` per character; the result
/// fits in the page cache after the first hit).
pub fn complete(input: &str) -> Completion {
    let expanded = expand_tilde(input);
    if expanded.is_empty() {
        return Completion::default();
    }
    let resolved = PathBuf::from(&expanded);

    // Decide parent vs prefix.
    //
    // We split on the LAST separator in the *raw expanded string* — NOT
    // via [`PathBuf::file_name`] / [`PathBuf::parent`]. Both of those
    // treat a trailing `.` / `..` as no-name (so `/tmp/foo/.` would
    // think the basename is `foo`), and we explicitly want the user's
    // typed `.` to act as the prefix when they're listing hidden dirs.
    //
    // - Input ending with `/` (or `\` on Windows): the user explicitly
    //   asked for "the children of this dir", so the whole resolved path
    //   is the parent and the prefix is empty.
    // - Otherwise: the substring after the last separator is the prefix
    //   and the substring before it is the parent. A bare `foo` (no
    //   separator) gets parent `.` (cwd) and prefix `foo`.
    let ends_with_sep = expanded.chars().last().map(is_separator).unwrap_or(false);

    let (parent, prefix) = if ends_with_sep {
        (resolved.clone(), String::new())
    } else if let Some(last_sep) = expanded.rfind(|c: char| is_separator(c)) {
        let (head, tail) = expanded.split_at(last_sep);
        // `tail` still contains the leading separator; strip it. `head`
        // can be empty for absolute roots like "/foo" — keep the
        // separator so the parent stays an absolute path.
        let prefix = tail
            .trim_start_matches(|c: char| is_separator(c))
            .to_string();
        let parent = if head.is_empty() {
            // `/foo` → head=""; parent is the root.
            PathBuf::from(std::path::MAIN_SEPARATOR_STR)
        } else {
            PathBuf::from(head)
        };
        (parent, prefix)
    } else {
        // No separator at all — bare name like "foo" relative to cwd.
        (PathBuf::from("."), expanded.clone())
    };

    let resolved_exists_as_dir = resolved.is_dir();
    let candidates = list_subdirs(&parent, &prefix);

    Completion {
        resolved,
        parent,
        prefix,
        candidates,
        resolved_exists_as_dir,
    }
}

/// True for any path separator the local OS understands.
fn is_separator(c: char) -> bool {
    c == '/' || (cfg!(target_os = "windows") && c == '\\')
}

/// Read `dir` and return the directory names (basenames) that start with
/// `prefix`. Hidden entries (names starting with `.`) are filtered out
/// unless the prefix itself starts with `.` (so the user can opt in to
/// hidden dirs by typing the dot explicitly). Non-directories are never
/// returned. The result is sorted lexicographically and capped at
/// [`Completion::CAP`].
fn list_subdirs(dir: &Path, prefix: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let case_insensitive = cfg!(target_os = "windows");
    let lower_prefix = prefix.to_lowercase();
    let show_hidden = prefix.starts_with('.');
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            // Skip entries we can't decide on; treating an unreadable
            // file_type as "not a directory" is the conservative call.
            let ft = e.file_type().ok()?;
            if !ft.is_dir() {
                // Follow symlinks one level so symlinked checkouts under
                // `~/code` are still completable. Avoid infinite loops by
                // only stat'ing the link target, not recursing into it.
                if ft.is_symlink() {
                    let meta = std::fs::metadata(e.path()).ok()?;
                    if !meta.is_dir() {
                        return None;
                    }
                } else {
                    return None;
                }
            }
            let name = e.file_name().into_string().ok()?;
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            let matches = if case_insensitive {
                name.to_lowercase().starts_with(&lower_prefix)
            } else {
                name.starts_with(prefix)
            };
            if !matches {
                return None;
            }
            Some(name)
        })
        .collect();
    out.sort();
    out.truncate(Completion::CAP);
    out
}

/// Compose the absolute path the App should commit when the user picks
/// `candidate` from the dropdown. Joins it under [`Completion::parent`]
/// (which already accounts for whether the input ended with `/`).
pub fn apply_candidate(comp: &Completion, candidate: &str) -> PathBuf {
    comp.parent.join(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tree(td: &TempDir, names: &[&str]) {
        for n in names {
            fs::create_dir_all(td.path().join(n)).unwrap();
        }
    }

    #[test]
    fn expand_tilde_passes_through_when_no_tilde() {
        assert_eq!(expand_tilde("/etc/foo"), "/etc/foo");
        assert_eq!(expand_tilde("foo"), "foo");
        assert_eq!(expand_tilde(""), "");
    }

    #[test]
    fn expand_tilde_replaces_leading_tilde_only() {
        // Override HOME so the test is deterministic regardless of the
        // user running it.
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(expand_tilde("~"), "/home/tester");
        assert_eq!(expand_tilde("~/code"), "/home/tester/code");
        // Other-user homes (`~someone`) are not expanded.
        assert_eq!(expand_tilde("~someone/x"), "~someone/x");
        // Tilde mid-string is literal, not expanded.
        assert_eq!(expand_tilde("/foo/~/bar"), "/foo/~/bar");
        if let Some(v) = prev {
            std::env::set_var("HOME", v);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn complete_lists_only_directories_filters_hidden() {
        let td = TempDir::new().unwrap();
        // A mix of dirs, files, and hidden entries.
        make_tree(&td, &["alpha", "alpaca", "beta", ".hidden"]);
        fs::write(td.path().join("plain.txt"), "x").unwrap();

        let base = format!("{}/", td.path().display());
        let comp = complete(&base);
        assert_eq!(comp.prefix, "");
        assert!(comp.resolved_exists_as_dir);
        assert_eq!(
            comp.candidates,
            vec!["alpaca".to_string(), "alpha".into(), "beta".into()],
            "files and hidden dirs filtered out, sorted lexicographically"
        );

        // Prefix narrows the list.
        let narrowed = complete(&format!("{}al", base));
        assert_eq!(narrowed.prefix, "al");
        assert_eq!(
            narrowed.candidates,
            vec!["alpaca".to_string(), "alpha".into()]
        );
        assert!(
            !narrowed.resolved_exists_as_dir,
            "the resolved path itself (…/al) is not a dir"
        );
    }

    #[test]
    fn complete_shows_hidden_when_user_explicitly_types_dot() {
        let td = TempDir::new().unwrap();
        make_tree(&td, &[".hidden", ".other", "visible"]);
        let comp = complete(&format!("{}/.", td.path().display()));
        assert_eq!(comp.prefix, ".");
        assert_eq!(
            comp.candidates,
            vec![".hidden".to_string(), ".other".into()]
        );
    }

    #[test]
    fn complete_resolves_tilde() {
        let td = TempDir::new().unwrap();
        make_tree(&td, &["work"]);
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", td.path());

        let comp = complete("~/");
        assert!(comp.resolved_exists_as_dir, "~/ resolves to HOME");
        assert!(comp.candidates.contains(&"work".to_string()));

        let narrowed = complete("~/wo");
        assert_eq!(narrowed.prefix, "wo");
        assert_eq!(narrowed.candidates, vec!["work".to_string()]);

        if let Some(v) = prev {
            std::env::set_var("HOME", v);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn complete_empty_input_returns_empty_completion() {
        let comp = complete("");
        assert!(!comp.resolved_exists_as_dir);
        assert!(comp.candidates.is_empty());
        assert_eq!(comp.parent, PathBuf::new());
    }

    #[test]
    fn complete_unreadable_parent_returns_empty_candidates_without_panic() {
        let comp = complete("/this/path/definitely/does/not/exist/abc");
        assert!(!comp.resolved_exists_as_dir);
        assert!(comp.candidates.is_empty(), "read_dir failure → no panic");
        assert_eq!(comp.prefix, "abc");
    }

    #[test]
    fn apply_candidate_joins_under_parent() {
        let td = TempDir::new().unwrap();
        make_tree(&td, &["proj-a"]);
        let comp = complete(&format!("{}/", td.path().display()));
        let chosen = apply_candidate(&comp, "proj-a");
        assert_eq!(chosen, td.path().join("proj-a"));
    }
}
