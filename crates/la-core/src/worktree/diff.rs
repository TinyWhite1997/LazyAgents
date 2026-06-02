//! Per-session diff review backend (M2.5 / WEK-28).
//!
//! Owns the git plumbing the daemon exposes via the `worktree.*` RPCs:
//!
//! - [`DiffEngine::status`] runs `git status --porcelain=v2 -z` against
//!   the session's worktree and returns a lightweight per-file
//!   snapshot.
//! - [`DiffEngine::diff_file`] runs `git diff` for one file (staged or
//!   unstaged) and parses the unified-diff stream via
//!   [`super::parser`]. Files > [`MAX_INLINE_DIFF_BYTES`] return a
//!   [`TruncationOutcome`] so the TUI can suggest "open in editor".
//! - [`DiffEngine::stage`] / [`DiffEngine::unstage`] /
//!   [`DiffEngine::discard`] take a list of `hunk_id` fingerprints,
//!   reconstruct a patch from the matched hunks (slicing the original
//!   `git diff` stdout — never re-emitting), and feed it to
//!   `git apply --cached [--reverse]` (stage/unstage) or
//!   `git apply --reverse` (discard, on the working tree).
//! - [`DiffEngine::commit`] runs `git commit -F -` with the message on
//!   stdin. Brief §3.4 cuts are honoured here: no `--amend`,
//!   `--signoff`, GPG flag toggling, or auto-push.
//! - [`DiffEngine::open_in_editor`] resolves an editor command and
//!   `spawn`s it. Fire-and-forget — the daemon does NOT wait.
//!
//! `WorktreeLocks` is the per-session `tokio::Mutex` map that brief
//! §3.5 mandates: every mutation takes the lock so concurrent
//! `git apply` invocations don't race on `.git/index.lock`. Reads
//! (`status` / `diff_file`) are lock-free — the TUI tolerates racy
//! snapshots and re-pulls on `worktree.changed` events.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::error::{CoreError, CoreResult};
use crate::session::SessionId;
use crate::worktree::git::{run_git, run_git_with_stdin, trim_stderr};
use crate::worktree::parser::{self, LineOrigin, ParsedDiff, ParsedFile, ParsedHunk};

/// Files larger than this are not diffed inline; `diff_file` returns a
/// [`TruncationOutcome`] with `reason = "too_large"` instead.
/// Matches brief §3.6 (5 MiB).
pub const MAX_INLINE_DIFF_BYTES: u64 = 5 * 1024 * 1024;

/// Per-session mutex registry — one [`tokio::sync::Mutex`] per
/// `session_id`, lazy-inserted on first mutation. Brief §3.5: stage /
/// unstage / discard / commit serialise per worktree so concurrent
/// callers cannot race on `.git/index.lock`. Reads are lock-free.
#[derive(Default, Clone)]
pub struct WorktreeLocks {
    inner: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
}

impl WorktreeLocks {
    pub fn new() -> Self {
        Self::default()
    }

    async fn for_session(&self, id: &SessionId) -> Arc<Mutex<()>> {
        let mut map = self.inner.lock().await;
        map.entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// One file in [`StatusSnapshot::files`]. Same shape as
/// `la_proto::methods::FileEntry`, but kept core-side so `la-core` does
/// not have to depend on the proto layout for internal callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub kind: FileKind,
    pub staged_hunks: u32,
    pub unstaged_hunks: u32,
    pub size_bytes: u64,
    pub mode_change: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    Text,
    Binary,
    Submodule,
    Symlink,
}

/// Output of `DiffEngine::status` — wire-equivalent to
/// `WorktreeStatusResult`, minus the `generated_at` timestamp which the
/// dispatcher stamps so `la-core` stays clock-free.
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub branch: String,
    pub base_branch: Option<String>,
    pub head: String,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileEntry>,
}

/// Returned in place of hunks for binary / large / submodule files.
#[derive(Debug, Clone)]
pub struct TruncationOutcome {
    pub reason: &'static str,
    pub size_bytes: u64,
    pub hint: &'static str,
}

/// One hunk inside [`DiffOutcome::hunks`]. `hunk_id` is the stable
/// fingerprint defined in brief §3.3 (see [`parser::compute_hunk_id`]).
/// `lines` mirrors `la_proto::methods::DiffLine`.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub hunk_id: String,
    pub staged: bool,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub origin: LineOrigin,
    pub content: String,
    pub no_newline: bool,
}

/// Output of `DiffEngine::diff_file`. `truncated` is `Some` only when
/// `hunks` is empty (binary or > [`MAX_INLINE_DIFF_BYTES`]).
#[derive(Debug, Clone)]
pub struct DiffOutcome {
    pub file: FileEntry,
    pub hunks: Vec<Hunk>,
    pub truncated: Option<TruncationOutcome>,
}

/// Output of stage / unstage / discard.
#[derive(Debug, Clone, Default)]
pub struct MutationOutcome {
    pub applied: Vec<String>,
    pub rejected: Vec<HunkReject>,
    pub status: Vec<FileEntry>,
    /// Paths touched by the mutation (used by the dispatcher to populate
    /// `worktree.changed.affected_files`).
    pub affected_files: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HunkReject {
    pub hunk_id: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub commit_sha: String,
    pub summary: String,
    pub files_changed: u32,
}

#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub launched: bool,
    pub command: String,
    pub pid: Option<u32>,
}

/// What kind of mutation `stage` / `unstage` / `discard` is performing
/// — used internally to pick the right `git apply` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    /// Index ← working tree (`git apply --cached`).
    Stage,
    /// Working tree ← index (`git apply --cached --reverse`).
    Unstage,
    /// Working tree ← HEAD (`git apply --reverse`, no `--cached`).
    Discard,
}

/// Public façade owned by the daemon. Cheap to clone — just a
/// few `Arc`s. Built lazily per RPC call by `SessionManager` so each
/// engine pins the right `worktree_path` for its session.
#[derive(Clone)]
pub struct DiffEngine {
    worktree_path: PathBuf,
    session_id: SessionId,
    locks: WorktreeLocks,
    /// Base branch recorded on the session row at create time. Used to
    /// compute ahead/behind in [`Self::status`]. `None` means the
    /// session row didn't capture one (e.g. older daemons or worktrees
    /// created outside the M2.4 path) — ahead/behind then degrade to
    /// `(0, 0)` rather than report a meaningless rev-list range.
    base_branch: Option<String>,
}

impl DiffEngine {
    pub fn new(
        _repo_root: PathBuf,
        worktree_path: PathBuf,
        session_id: SessionId,
        locks: WorktreeLocks,
        base_branch: Option<String>,
    ) -> Self {
        Self {
            worktree_path,
            session_id,
            locks,
            base_branch,
        }
    }

    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    /// Lightweight per-file snapshot. Reads `git status --porcelain=v2
    /// -z`, `git rev-parse HEAD`, and the ahead/behind counts versus
    /// the recorded base branch.
    pub async fn status(&self) -> CoreResult<StatusSnapshot> {
        let head = self.head_sha().await?;
        let branch = self.current_branch().await?;
        let porcelain = run_git(
            &self.worktree_path,
            &["status", "--porcelain=v2", "-z", "--untracked-files=normal"],
        )
        .await?;
        if !porcelain.success {
            return Err(CoreError::WorktreeProvision {
                stderr: trim_stderr(&porcelain.stderr),
            });
        }
        let entries = parse_porcelain_v2(&porcelain.stdout_bytes);

        // Resolve sizes from the working tree (untracked included).
        let mut files = Vec::with_capacity(entries.len());
        for raw in entries {
            let size = if matches!(raw.status, FileStatus::Deleted) {
                0
            } else {
                tokio::fs::metadata(self.worktree_path.join(&raw.path))
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0)
            };
            files.push(FileEntry {
                path: raw.path,
                old_path: raw.old_path,
                status: raw.status,
                kind: raw.kind,
                staged_hunks: raw.staged_hunks,
                unstaged_hunks: raw.unstaged_hunks,
                size_bytes: size,
                mode_change: raw.mode_change,
            });
        }

        let (ahead, behind) = if let Some(base) = self.base_branch.as_deref() {
            self.ahead_behind(&branch, base).await.unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        let base_branch = self.base_branch.clone();

        Ok(StatusSnapshot {
            branch,
            base_branch,
            head,
            ahead,
            behind,
            files,
        })
    }

    /// Per-file diff. Returns a [`TruncationOutcome`] for binaries,
    /// submodules, and files larger than [`MAX_INLINE_DIFF_BYTES`].
    pub async fn diff_file(
        &self,
        path: &str,
        staged: bool,
        context_lines: Option<u32>,
    ) -> CoreResult<DiffOutcome> {
        let abs_path = self.worktree_path.join(path);
        let size = tokio::fs::metadata(&abs_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        // Re-pull the file row from status so we can stuff the
        // up-to-date `FileEntry` into the result without making the
        // caller round-trip a second `status` call.
        let snapshot = self.status().await?;
        let entry = snapshot
            .files
            .into_iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| FileEntry {
                path: path.to_string(),
                old_path: None,
                status: FileStatus::Modified,
                kind: FileKind::Text,
                staged_hunks: 0,
                unstaged_hunks: 0,
                size_bytes: size,
                mode_change: None,
            });

        if matches!(entry.kind, FileKind::Binary) {
            return Ok(DiffOutcome {
                file: entry,
                hunks: vec![],
                truncated: Some(TruncationOutcome {
                    reason: "binary",
                    size_bytes: size,
                    hint: "open_in_editor",
                }),
            });
        }
        if matches!(entry.kind, FileKind::Submodule) {
            return Ok(DiffOutcome {
                file: entry,
                hunks: vec![],
                truncated: Some(TruncationOutcome {
                    reason: "submodule",
                    size_bytes: size,
                    hint: "open_in_editor",
                }),
            });
        }
        if size > MAX_INLINE_DIFF_BYTES {
            return Ok(DiffOutcome {
                file: entry,
                hunks: vec![],
                truncated: Some(TruncationOutcome {
                    reason: "too_large",
                    size_bytes: size,
                    hint: "open_in_editor",
                }),
            });
        }

        // Untracked files have no diff history; show synthetic
        // "everything added" by feeding `git diff --no-index /dev/null
        // <path>`.
        let parsed = if matches!(entry.status, FileStatus::Untracked) && !staged {
            self.diff_untracked(path, context_lines).await?
        } else {
            self.git_diff(path, staged, context_lines).await?
        };

        let hunks = parsed
            .files
            .into_iter()
            .next()
            .map(|f| build_hunks(&f, staged))
            .unwrap_or_default();

        Ok(DiffOutcome {
            file: entry,
            hunks,
            truncated: None,
        })
    }

    pub async fn stage(&self, hunk_ids: &[String]) -> CoreResult<MutationOutcome> {
        self.apply_mutation(hunk_ids, MutationKind::Stage).await
    }

    pub async fn unstage(&self, hunk_ids: &[String]) -> CoreResult<MutationOutcome> {
        self.apply_mutation(hunk_ids, MutationKind::Unstage).await
    }

    pub async fn discard(&self, hunk_ids: &[String]) -> CoreResult<MutationOutcome> {
        self.apply_mutation(hunk_ids, MutationKind::Discard).await
    }

    /// Run `git commit -F -` with `message` on stdin. Honours every
    /// pre-commit / commit-msg hook in the repo's git config; the
    /// daemon never adds `--no-verify`. Brief §3.4 explicitly cuts:
    /// `--amend`, `--signoff`, GPG flag toggling, auto-push.
    pub async fn commit(&self, message: &str, allow_empty: bool) -> CoreResult<CommitOutcome> {
        let lock = self.locks.for_session(&self.session_id).await;
        let _guard = lock.lock().await;
        let mut args: Vec<&str> = vec!["commit", "-F", "-"];
        if allow_empty {
            args.push("--allow-empty");
        }
        let out = run_git_with_stdin(&self.worktree_path, &args, Some(message.as_bytes())).await?;
        if !out.success {
            return Err(classify_commit_error(&out.stderr, out.exit_code));
        }
        let sha = self.head_sha().await?;
        let summary = message.lines().next().unwrap_or("").to_string();
        let count_out = run_git(
            &self.worktree_path,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", &sha],
        )
        .await?;
        let files_changed = if count_out.success {
            count_out
                .stdout
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count() as u32
        } else {
            0
        };
        Ok(CommitOutcome {
            commit_sha: sha,
            summary,
            files_changed,
        })
    }

    /// Resolve `$VISUAL` / `$EDITOR` / `code` and `spawn` it pointed at
    /// `path`. Returns as soon as `spawn()` succeeds — fire-and-forget
    /// by design (brief §3.2). Never waits, never reaps.
    pub async fn open_in_editor(
        &self,
        path: &str,
        line: Option<u32>,
        column: Option<u32>,
        editor_override: Option<&str>,
    ) -> CoreResult<LaunchOutcome> {
        let editor = match editor_override {
            Some(e) if !e.trim().is_empty() => e.to_string(),
            _ => match std::env::var("VISUAL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    std::env::var("EDITOR")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                }) {
                Some(e) => e,
                None => {
                    if which_in_path("code").is_some() {
                        "code".to_string()
                    } else {
                        return Err(CoreError::WorktreeEditorUnavailable);
                    }
                }
            },
        };

        let target = self.worktree_path.join(path);
        let argv = editor_argv(&editor, &target, line, column);
        if argv.is_empty() {
            return Err(CoreError::WorktreeEditorUnavailable);
        }

        let mut cmd = std::process::Command::new(&argv[0]);
        for a in argv.iter().skip(1) {
            cmd.arg(a);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let child = cmd.spawn().map_err(CoreError::WorktreeIo)?;
        let pid = Some(child.id());
        // Detach: never `wait()`. The OS reaps the child when it exits.
        // (We could log the eventual exit code on a background task; M2
        // doesn't need it.)
        std::mem::drop(child);

        let command_str = argv.join(" ");
        Ok(LaunchOutcome {
            launched: true,
            command: command_str,
            pid,
        })
    }

    // ----- internals -----

    async fn apply_mutation(
        &self,
        hunk_ids: &[String],
        kind: MutationKind,
    ) -> CoreResult<MutationOutcome> {
        let lock = self.locks.for_session(&self.session_id).await;
        let _guard = lock.lock().await;
        if hunk_ids.is_empty() {
            let status = self.status().await?;
            return Ok(MutationOutcome {
                applied: vec![],
                rejected: vec![],
                status: status.files,
                affected_files: vec![],
            });
        }

        let id_set: std::collections::HashSet<&str> = hunk_ids.iter().map(String::as_str).collect();
        let mut applied = Vec::new();
        let mut rejected = Vec::new();
        let mut affected = Vec::new();

        // Per the brief, we always re-read the diff and recompute ids
        // — a stale TUI must not be able to apply a hunk whose body
        // has shifted under it. We group by file to build one patch
        // per file, applied under the single lock we already hold.
        let snapshot = self.status().await?;
        let candidate_files: BTreeMap<String, &FileEntry> =
            snapshot.files.iter().map(|f| (f.path.clone(), f)).collect();

        for (path, entry) in candidate_files {
            if matches!(entry.kind, FileKind::Binary | FileKind::Submodule) {
                continue;
            }

            // For each file, fetch the appropriate diff source.
            let (diff_buf, staged_flag) = match kind {
                MutationKind::Stage => {
                    if matches!(entry.status, FileStatus::Untracked) {
                        // `git apply --cached` cannot stage from an
                        // untracked file directly; mark with
                        // `--intent-to-add` so the diff machinery sees
                        // it as a tracked-empty-vs-content diff.
                        let _ = run_git(
                            &self.worktree_path,
                            &["add", "--intent-to-add", "--", &path],
                        )
                        .await;
                    }
                    (self.raw_diff(&path, false).await?, false)
                }
                MutationKind::Unstage => (self.raw_diff(&path, true).await?, true),
                MutationKind::Discard => (self.raw_diff(&path, false).await?, false),
            };
            let parsed = parser::parse(&diff_buf);
            let parsed_file = match parsed.files.into_iter().next() {
                Some(f) => f,
                None => continue,
            };
            // ID computation MUST match build_hunks (used by diff_file)
            // so the TUI's saved ids round-trip — both paths go through
            // the same reconstructed body bytes here.
            let canonical_hunks = build_hunks(&parsed_file, matches!(kind, MutationKind::Unstage));
            let mut selected: Vec<(&ParsedHunk, String)> = Vec::new();
            for (parsed_hunk, computed) in parsed_file.hunks.iter().zip(canonical_hunks.iter()) {
                if id_set.contains(computed.hunk_id.as_str()) {
                    selected.push((parsed_hunk, computed.hunk_id.clone()));
                }
            }
            if selected.is_empty() {
                continue;
            }
            // Build a patch fragment: file header + selected hunk
            // bodies, byte-spliced from the source.
            let mut patch: Vec<u8> = Vec::new();
            let hdr = &diff_buf[parsed_file.header_range.0..parsed_file.header_range.1];
            patch.extend_from_slice(hdr);
            // For untracked-staged paths git emits `new file mode` /
            // empty `---`; trust the parsed header.
            for (hunk, _) in &selected {
                patch.extend_from_slice(&diff_buf[hunk.body_range.0..hunk.body_range.1]);
            }

            let mut args: Vec<&str> = vec!["apply"];
            match kind {
                MutationKind::Stage => {
                    args.push("--cached");
                }
                MutationKind::Unstage => {
                    args.push("--cached");
                    args.push("--reverse");
                }
                MutationKind::Discard => {
                    args.push("--reverse");
                }
            }
            args.push("--whitespace=nowarn");
            args.push("-");

            let out = run_git_with_stdin(&self.worktree_path, &args, Some(&patch)).await?;
            if out.success {
                for (_, id) in &selected {
                    applied.push(id.clone());
                }
                affected.push(path.clone());
                // If we discarded every hunk of a file that started as
                // untracked, also `git rm --cached` to restore the
                // "untracked" state cleanly (brief risk §5.3).
                if matches!(kind, MutationKind::Unstage)
                    && matches!(entry.status, FileStatus::Added)
                    && selected.len() == parsed_file.hunks.len()
                {
                    let _ = run_git(&self.worktree_path, &["rm", "--cached", "--", &path]).await;
                }
                // Discard of an untracked file: also `rm` it from disk
                // (the patch only reverses content, not file
                // existence).
                if matches!(kind, MutationKind::Discard)
                    && matches!(entry.status, FileStatus::Untracked)
                {
                    let _ = tokio::fs::remove_file(self.worktree_path.join(&path)).await;
                }
                let _ = staged_flag;
            } else {
                // `git apply` refused. Two distinct failure modes; the
                // wire-level reason has to reflect which one:
                //   - "patch does not apply" / "corrupt patch" → the
                //     index moved beneath us between read and write.
                //     Surface as `patch_rejected` so the TUI can show
                //     git's stderr and prompt a refresh.
                //   - anything else (locked index, IO error mid-apply,
                //     unknown) → fall back to `stale` so the TUI
                //     defaults to "re-pull status and retry", which is
                //     the safer recovery path.
                //
                // The wire-side code `-33125 WORKTREE_PATCH_REJECTED` is
                // reserved for a future "force_full = true" override
                // that would `Err` the whole call; the per-hunk reason
                // above is what reaches a normal TUI mutation.
                let reason = if stderr_means_patch_rejected(&out.stderr) {
                    "patch_rejected"
                } else {
                    "stale"
                };
                for (_, id) in &selected {
                    rejected.push(HunkReject {
                        hunk_id: id.clone(),
                        reason,
                    });
                }
            }
        }

        // Re-pull status so the caller sees the post-mutation FileEntry
        // snapshot in one round trip.
        let status = self.status().await?;
        if applied.is_empty() && rejected.is_empty() && !hunk_ids.is_empty() {
            // None of the supplied ids matched any live hunk → return
            // all as stale (callers will treat this as a signal to
            // refresh).
            return Ok(MutationOutcome {
                applied: vec![],
                rejected: hunk_ids
                    .iter()
                    .map(|id| HunkReject {
                        hunk_id: id.clone(),
                        reason: "stale",
                    })
                    .collect(),
                status: status.files,
                affected_files: vec![],
            });
        }
        Ok(MutationOutcome {
            applied,
            rejected,
            status: status.files,
            affected_files: affected,
        })
    }

    async fn raw_diff(&self, path: &str, staged: bool) -> CoreResult<Vec<u8>> {
        // Match the default `-U3` of `diff_file` so hunk ids
        // round-trip from a TUI `worktree.diff` call into a
        // `worktree.stage` call. Mismatched context counts produce
        // different hunk bodies → different ids → spurious "stale"
        // rejects.
        let mut args: Vec<String> = vec![
            "diff".into(),
            "--no-color".into(),
            "--no-ext-diff".into(),
            "--find-renames".into(),
            "-U3".into(),
        ];
        if staged {
            args.push("--cached".into());
        }
        args.push("--".into());
        args.push(path.into());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_git(&self.worktree_path, &arg_refs).await?;
        if !out.success && out.exit_code != Some(1) {
            return Err(CoreError::WorktreeProvision {
                stderr: trim_stderr(&out.stderr),
            });
        }
        Ok(out.stdout_bytes)
    }

    async fn git_diff(
        &self,
        path: &str,
        staged: bool,
        context_lines: Option<u32>,
    ) -> CoreResult<ParsedDiff> {
        let ctx = context_lines.unwrap_or(3);
        let ctx_arg = format!("-U{ctx}");
        let mut args: Vec<&str> = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            &ctx_arg,
        ];
        if staged {
            args.push("--cached");
        }
        args.push("--");
        args.push(path);
        let out = run_git(&self.worktree_path, &args).await?;
        if !out.success && out.exit_code != Some(1) {
            // exit 1 means "found differences" for `git diff` — not a
            // failure.
            return Err(CoreError::WorktreeProvision {
                stderr: trim_stderr(&out.stderr),
            });
        }
        Ok(parser::parse(&out.stdout_bytes))
    }

    /// Synthetic diff for an untracked file: treat it as a fresh-add
    /// against an empty blob. We construct the patch ourselves rather
    /// than depending on `git diff --no-index` because the latter
    /// returns paths relative to cwd, which clashes with our
    /// `git -C <worktree>` discipline.
    async fn diff_untracked(
        &self,
        path: &str,
        _context_lines: Option<u32>,
    ) -> CoreResult<ParsedDiff> {
        let abs = self.worktree_path.join(path);
        let bytes = tokio::fs::read(&abs).await.map_err(CoreError::WorktreeIo)?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                // Treat as binary — caller handles via FileKind::Binary
                // upstream of this path.
                let mut header = format!(
                    "diff --git a/{p} b/{p}\nnew file mode 100644\nindex 0000000..0000000\nBinary files /dev/null and b/{p} differ\n",
                    p = path
                );
                header.shrink_to_fit();
                return Ok(parser::parse(header.as_bytes()));
            }
        };
        let mut out = format!(
            "diff --git a/{p} b/{p}\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/{p}\n",
            p = path
        );
        let line_iter: Vec<&str> = text.split_inclusive('\n').collect();
        let n = line_iter.len();
        if n == 0 {
            return Ok(parser::parse(out.as_bytes()));
        }
        let trailing_nl = text.ends_with('\n');
        let new_count = n as u32;
        out.push_str(&format!("@@ -0,0 +1,{new_count} @@\n"));
        for (i, line) in line_iter.iter().enumerate() {
            let last = i + 1 == n;
            if last && !trailing_nl {
                out.push_str(&format!("+{}\n\\ No newline at end of file\n", line));
            } else {
                out.push('+');
                out.push_str(line);
                if !line.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        Ok(parser::parse(out.as_bytes()))
    }

    async fn head_sha(&self) -> CoreResult<String> {
        let out = run_git(&self.worktree_path, &["rev-parse", "HEAD"]).await?;
        if !out.success {
            return Ok(String::new());
        }
        Ok(out.stdout.trim().to_string())
    }

    async fn current_branch(&self) -> CoreResult<String> {
        let out = run_git(&self.worktree_path, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        if !out.success {
            return Ok(String::from("HEAD"));
        }
        Ok(out.stdout.trim().to_string())
    }

    async fn ahead_behind(&self, branch: &str, base: &str) -> Option<(u32, u32)> {
        let range = format!("{base}...{branch}");
        let out = run_git(
            &self.worktree_path,
            &["rev-list", "--left-right", "--count", &range],
        )
        .await
        .ok()?;
        if !out.success {
            return None;
        }
        let mut parts = out.stdout.split_whitespace();
        let behind = parts.next()?.parse().ok()?;
        let ahead = parts.next()?.parse().ok()?;
        Some((ahead, behind))
    }
}

/// `true` when git's stderr from a failed `git apply --cached` /
/// `--reverse` invocation indicates the patch itself was rejected
/// against the current index, as opposed to a transient IO / lock
/// error. The TUI uses this to choose between `patch_rejected`
/// (refresh + surface git's reason) and `stale` (refresh + retry).
fn stderr_means_patch_rejected(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("patch does not apply")
        || s.contains("corrupt patch")
        || s.contains("error while searching for")
        || s.contains("patch failed")
}

fn classify_commit_error(stderr: &str, exit_code: Option<i32>) -> CoreError {
    let s = stderr.to_ascii_lowercase();
    if s.contains("nothing to commit")
        || s.contains("no changes added to commit")
        || s.contains("nothing added to commit")
    {
        return CoreError::WorktreeCommitEmpty;
    }
    if exit_code.unwrap_or(0) != 0 {
        // Anything non-zero past the empty-commit shortcut is either a
        // hook failure or a config/index failure — surface stderr to
        // the TUI either way; the WORKTREE_COMMIT_HOOK_FAILED code
        // covers both.
        return CoreError::WorktreeCommitHookFailed {
            stderr: trim_stderr(stderr),
        };
    }
    CoreError::WorktreeProvision {
        stderr: trim_stderr(stderr),
    }
}

fn build_hunks(file: &ParsedFile, staged: bool) -> Vec<Hunk> {
    file.hunks
        .iter()
        .map(|h| {
            // hunk body for the id must include the `@@` header so the
            // id reflects header drift; the parser already stored the
            // body_range starting at the header.
            //
            // We rebuild a representative body for id stability across
            // re-reads: header line + each tagged line + optional
            // \ No newline marker. The parser is byte-faithful but
            // does not preserve trailing `\n` count on the last line
            // when re-emitting; for the id we hash the structure.
            let mut body = String::with_capacity(h.header.len() + 64);
            body.push_str(&h.header);
            body.push('\n');
            for l in &h.lines {
                let prefix = match l.origin {
                    LineOrigin::Context => ' ',
                    LineOrigin::Add => '+',
                    LineOrigin::Delete => '-',
                };
                body.push(prefix);
                body.push_str(&l.content);
                body.push('\n');
                if l.no_newline {
                    body.push_str("\\ No newline at end of file\n");
                }
            }
            let hunk_id =
                parser::compute_hunk_id(&file.new_path, h.old_start, h.old_count, body.as_bytes());
            Hunk {
                hunk_id,
                staged,
                old_start: h.old_start,
                old_count: h.old_count,
                new_start: h.new_start,
                new_count: h.new_count,
                header: h.header.clone(),
                lines: h
                    .lines
                    .iter()
                    .map(|l| DiffLine {
                        origin: l.origin,
                        content: l.content.clone(),
                        no_newline: l.no_newline,
                    })
                    .collect(),
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
struct PorcelainEntry {
    path: String,
    old_path: Option<String>,
    status: FileStatus,
    kind: FileKind,
    staged_hunks: u32,
    unstaged_hunks: u32,
    mode_change: Option<(u32, u32)>,
}

/// Parse `git status --porcelain=v2 -z` output into per-file entries.
///
/// porcelain v2 emits NUL-separated records. The shapes we accept:
/// - `1 XY <sub> <mH> <mI> <mW> <hH> <hI> <path>\0`
/// - `2 XY <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>\0<orig>\0`
/// - `? <path>\0`        (untracked)
/// - `u XY ...`           (conflicted)
fn parse_porcelain_v2(buf: &[u8]) -> Vec<PorcelainEntry> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    // When the previous record was a `2` rename header, the next NUL
    // field is the original path; we consume it here instead of
    // treating it as a new record.
    let mut expecting_old_path = false;
    while cursor < buf.len() {
        let end = buf[cursor..]
            .iter()
            .position(|b| *b == 0)
            .map(|i| cursor + i)
            .unwrap_or(buf.len());
        let record = &buf[cursor..end];
        cursor = end + 1;
        if record.is_empty() {
            continue;
        }
        let s = match std::str::from_utf8(record) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if expecting_old_path {
            if let Some(last) = out.last_mut() {
                let last: &mut PorcelainEntry = last;
                last.old_path = Some(s.to_string());
            }
            expecting_old_path = false;
            continue;
        }
        let mut parts = s.splitn(2, ' ');
        let tag = parts.next().unwrap_or("");
        match tag {
            "1" => {
                if let Some(rest) = parts.next() {
                    if let Some(entry) = parse_change_v2(rest, false) {
                        out.push(entry);
                    }
                }
            }
            "2" => {
                if let Some(rest) = parts.next() {
                    if let Some(entry) = parse_change_v2(rest, true) {
                        out.push(entry);
                        expecting_old_path = true;
                    }
                }
            }
            "?" => {
                if let Some(rest) = parts.next() {
                    out.push(PorcelainEntry {
                        path: rest.to_string(),
                        old_path: None,
                        status: FileStatus::Untracked,
                        kind: FileKind::Text,
                        staged_hunks: 0,
                        unstaged_hunks: 1,
                        mode_change: None,
                    });
                }
            }
            "u" => {
                if let Some(rest) = parts.next() {
                    if let Some(path) = rest.split_whitespace().last() {
                        out.push(PorcelainEntry {
                            path: path.to_string(),
                            old_path: None,
                            status: FileStatus::Conflicted,
                            kind: FileKind::Text,
                            staged_hunks: 0,
                            unstaged_hunks: 0,
                            mode_change: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_change_v2(rest: &str, is_rename: bool) -> Option<PorcelainEntry> {
    // Field layout per `git status --porcelain=v2` docs:
    // `1` records: XY sub mH mI mW hH hI path
    // `2` records: XY sub mH mI mW hH hI X<score> path
    let mut tokens = rest.split_whitespace();
    let xy = tokens.next()?;
    let _sub = tokens.next()?;
    let mh = tokens.next()?;
    let mi = tokens.next()?;
    let _mw = tokens.next()?;
    let _hh = tokens.next()?;
    let _hi = tokens.next()?;
    let mut path_tokens: Vec<&str> = tokens.collect();
    // Only `2` records carry the rename/copy score in front of the
    // path — guard on `is_rename` so a path that legitimately starts
    // with 'R'/'C' (like `README.md`) isn't mistaken for it.
    if is_rename
        && path_tokens
            .first()
            .map(|t| {
                let bytes = t.as_bytes();
                (bytes.first() == Some(&b'R') || bytes.first() == Some(&b'C'))
                    && bytes.iter().skip(1).all(|c| c.is_ascii_digit())
            })
            .unwrap_or(false)
    {
        path_tokens.remove(0);
    }
    if path_tokens.is_empty() {
        return None;
    }
    let path = path_tokens.join(" ");

    let x = xy.chars().next().unwrap_or('.');
    let y = xy.chars().nth(1).unwrap_or('.');
    let staged_hunks = if x == '.' { 0 } else { 1 };
    let unstaged_hunks = if y == '.' { 0 } else { 1 };
    let status = classify_xy(x, y);
    let mode_change = parse_mode_change(mh, mi);

    Some(PorcelainEntry {
        path,
        old_path: None,
        status,
        kind: classify_kind(mh, mi),
        staged_hunks,
        unstaged_hunks,
        mode_change,
    })
}

fn classify_xy(x: char, y: char) -> FileStatus {
    let primary = if x != '.' { x } else { y };
    match primary {
        'A' => FileStatus::Added,
        'M' => FileStatus::Modified,
        'D' => FileStatus::Deleted,
        'R' => FileStatus::Renamed,
        'C' => FileStatus::Copied,
        'U' => FileStatus::Conflicted,
        _ => FileStatus::Modified,
    }
}

fn classify_kind(mh: &str, mi: &str) -> FileKind {
    // mode `160000` ⇒ submodule; `120000` ⇒ symlink; everything else
    // we treat as text and let the diff path classify via `Binary
    // files differ` if needed.
    let mode = if mi != "000000" { mi } else { mh };
    match mode {
        "160000" => FileKind::Submodule,
        "120000" => FileKind::Symlink,
        _ => FileKind::Text,
    }
}

fn parse_mode_change(mh: &str, mi: &str) -> Option<(u32, u32)> {
    let h = u32::from_str_radix(mh, 8).ok()?;
    let i = u32::from_str_radix(mi, 8).ok()?;
    if h != 0 && i != 0 && h != i {
        Some((h, i))
    } else {
        None
    }
}

fn editor_argv(editor: &str, target: &Path, line: Option<u32>, column: Option<u32>) -> Vec<String> {
    // Split on whitespace so `$EDITOR="code --wait"` keeps the wait
    // flag. Quoting inside `$EDITOR` is intentionally NOT supported
    // (matches `git`'s own behaviour).
    let mut argv: Vec<String> = editor.split_whitespace().map(String::from).collect();
    if argv.is_empty() {
        return argv;
    }
    let basename = std::path::Path::new(&argv[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&argv[0])
        .to_ascii_lowercase();
    let target_str = target.to_string_lossy().into_owned();
    match basename.as_str() {
        "code" | "code-insiders" | "cursor" | "vscodium" | "windsurf" => {
            argv.push("--goto".into());
            argv.push(line_col_target(&target_str, line, column));
        }
        "zed" | "zeditor" => {
            argv.push(line_col_target(&target_str, line, column));
        }
        "idea" | "pycharm" | "webstorm" | "rustrover" | "clion" => {
            if let Some(l) = line {
                argv.push("--line".into());
                argv.push(l.to_string());
            }
            argv.push(target_str);
        }
        "vim" | "nvim" | "vi" => {
            if let Some(l) = line {
                argv.push(format!("+{l}"));
            }
            argv.push(target_str);
        }
        "emacs" | "emacsclient" => {
            if let Some(l) = line {
                argv.push(format!("+{l}"));
            }
            argv.push(target_str);
        }
        _ => {
            argv.push(target_str);
        }
    }
    argv
}

fn line_col_target(target: &str, line: Option<u32>, column: Option<u32>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!("{target}:{l}:{c}"),
        (Some(l), None) => format!("{target}:{l}"),
        _ => target.to_string(),
    }
}

#[cfg(unix)]
fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for p in std::env::split_paths(&path) {
        let candidate = p.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let candidates = [name.to_string(), format!("{name}.exe")];
    for p in std::env::split_paths(&path) {
        for c in &candidates {
            let candidate = p.join(c);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_argv_handles_vscode() {
        let argv = editor_argv("code", Path::new("/tmp/foo.rs"), Some(12), Some(4));
        assert_eq!(argv, vec!["code", "--goto", "/tmp/foo.rs:12:4"]);
    }

    #[test]
    fn editor_argv_handles_vim_with_line() {
        let argv = editor_argv("vim", Path::new("/tmp/foo.rs"), Some(7), None);
        assert_eq!(argv, vec!["vim", "+7", "/tmp/foo.rs"]);
    }

    #[test]
    fn editor_argv_passes_through_for_unknown_editor() {
        let argv = editor_argv("ed", Path::new("/tmp/foo.rs"), None, None);
        assert_eq!(argv, vec!["ed", "/tmp/foo.rs"]);
    }

    #[test]
    fn editor_argv_respects_extra_args() {
        let argv = editor_argv("code --wait", Path::new("/tmp/foo.rs"), None, None);
        assert_eq!(argv, vec!["code", "--wait", "--goto", "/tmp/foo.rs"]);
    }

    #[test]
    fn classify_xy_recognises_basic_cases() {
        assert_eq!(classify_xy('A', '.'), FileStatus::Added);
        assert_eq!(classify_xy('.', 'M'), FileStatus::Modified);
        assert_eq!(classify_xy('D', '.'), FileStatus::Deleted);
        assert_eq!(classify_xy('R', '.'), FileStatus::Renamed);
        assert_eq!(classify_xy('U', 'U'), FileStatus::Conflicted);
    }

    #[test]
    fn parse_mode_change_picks_real_changes_only() {
        assert_eq!(
            parse_mode_change("100644", "100755"),
            Some((0o100644, 0o100755))
        );
        assert_eq!(parse_mode_change("100644", "100644"), None);
        assert_eq!(parse_mode_change("100644", "000000"), None);
    }

    #[test]
    fn stderr_means_patch_rejected_detects_real_failures() {
        assert!(stderr_means_patch_rejected(
            "error: patch failed: src/foo.rs:12\nerror: src/foo.rs: patch does not apply\n"
        ));
        assert!(stderr_means_patch_rejected(
            "fatal: corrupt patch at line 5"
        ));
        // Non-patch-rejection errors must not match — they fall back
        // to the "stale, refresh + retry" path.
        assert!(!stderr_means_patch_rejected(
            "fatal: Unable to create '/repo/.git/index.lock': File exists.\n"
        ));
        assert!(!stderr_means_patch_rejected(""));
    }
}
