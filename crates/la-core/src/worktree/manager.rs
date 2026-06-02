//! `WorktreeManager` — high-level façade for per-session worktrees.
//!
//! This module covers:
//!
//! - Creating a fresh worktree under
//!   `<state_dir>/worktrees/<project_slug>/<short_sid>` rooted at the
//!   resolved base SHA, with atomic rollback on failure.
//! - Running the advisory `.lazyagents/hooks/post-create.sh` script with a
//!   60 s budget (success / fail / timeout / skipped sentinel).
//! - Tearing the worktree down on archive / sweep, preserving any branch
//!   that still has un-merged commits (see [`CleanupMode`]).
//! - Best-effort `git worktree prune` on daemon startup.
//!
//! All git plumbing is delegated to [`super::git`]; this file should not
//! shell out directly. Error classification is centralised in
//! [`super::git::classify_add_error`] (brief R3).

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::{CoreError, CoreResult};
use crate::worktree::git;

/// Wall-clock budget for the post-create hook (brief §2.5). Hooks that run
/// past this point are killed and recorded as `timeout`; the worktree
/// itself is preserved per the "hook is advisory" rule.
pub const POST_CREATE_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// Hook script path inside a project repo. Brief R2 pins this name; any
/// configurable override is M3 work.
pub const HOOK_RELATIVE_PATH: &str = ".lazyagents/hooks/post-create.sh";

/// Hook execution outcome surfaced to storage as
/// `post_create_hook_status`. The string form matches the SQLite CHECK
/// constraint added in migration `0002`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStatus {
    /// Hook ran and returned 0.
    Ok,
    /// Hook ran and returned non-zero (or wait failed). Worktree is kept.
    Failed,
    /// Hook ran past [`POST_CREATE_HOOK_TIMEOUT`] and was killed.
    Timeout,
    /// No hook present, or present but not executable.
    Skipped,
}

impl HookStatus {
    /// String form persisted in `sessions.post_create_hook_status`.
    pub fn as_str(self) -> &'static str {
        match self {
            HookStatus::Ok => "ok",
            HookStatus::Failed => "failed",
            HookStatus::Timeout => "timeout",
            HookStatus::Skipped => "skipped",
        }
    }
}

/// Strategy for [`WorktreeManager::cleanup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupMode {
    /// Remove the worktree directory but keep the branch if it has commits
    /// beyond `base_branch` — the user can still `git checkout` it later
    /// to recover work. Default for `sessions.archive`.
    KeepBranchIfDirty,
    /// Remove the worktree directory and the branch unconditionally.
    /// Used when the worktree never produced commits.
    Remove,
    /// Pass `--force` to `git worktree remove`; used for rollback of a
    /// half-created worktree where the in-process branch may still be in
    /// an inconsistent state.
    Force,
}

/// Plan returned by [`WorktreeManager::create`] before the session row is
/// persisted. Carries the four fields `NewSession` needs and is consumed
/// in one shot — `WorktreeManager` does not retain a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePlan {
    /// Absolute path to the new worktree directory.
    pub path: PathBuf,
    /// Newly created branch (`la/session-<short_sid>`).
    pub branch: String,
    /// Resolved base branch the worktree was forked from (short form).
    pub base_branch: String,
    /// 40-char SHA of `base_branch` at fork time.
    pub base_sha: String,
}

/// Reverse-constructed handle used for cleanup / sweep operations. Carries
/// just enough to drive `git worktree remove` + branch deletion; does not
/// hold a lock or open file descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    /// Repo working tree the worktree was forked from. `git` runs with
    /// this as `-C`.
    pub repo_root: PathBuf,
    /// Worktree directory to delete.
    pub worktree_path: PathBuf,
    /// Branch name (`la/session-<sid>`).
    pub branch: String,
    /// Original `base_branch` at create time. Used to detect whether the
    /// branch has commits beyond it so cleanup can decide whether to drop
    /// the branch in [`CleanupMode::KeepBranchIfDirty`].
    pub base_branch: String,
}

/// Manages the worktree root under which every per-session worktree lives.
/// Cheap to clone — internally a single [`PathBuf`].
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    /// `<state_dir>/worktrees` — parent of every per-session directory.
    worktrees_root: PathBuf,
    /// Wall-clock budget for the post-create hook. Tests shrink this
    /// via [`WorktreeManager::for_state_dir_with_hook_timeout`] so the
    /// `Timeout` assertion doesn't sit for a full minute on CI.
    hook_timeout: Duration,
}

impl WorktreeManager {
    /// Build a manager rooted under `worktrees_root`. The directory is
    /// **not** created here; [`WorktreeManager::create`] makes any missing
    /// parents lazily so a daemon that never spawns a worktree session
    /// doesn't leave empty scaffolding on disk.
    pub fn new(worktrees_root: impl Into<PathBuf>) -> Self {
        Self {
            worktrees_root: worktrees_root.into(),
            hook_timeout: POST_CREATE_HOOK_TIMEOUT,
        }
    }

    /// Convenience: build a manager whose `worktrees_root` lives directly
    /// under the daemon's `state_dir` (`$XDG_DATA_HOME/lazyagents`).
    pub fn for_state_dir(state_dir: impl AsRef<Path>) -> Self {
        Self::new(state_dir.as_ref().join("worktrees"))
    }

    /// Same as [`for_state_dir`] but with a custom hook wall-clock
    /// budget. Production callers should stick to
    /// [`POST_CREATE_HOOK_TIMEOUT`]; tests use this to validate the
    /// `Timeout` arm without sleeping for a real minute.
    pub fn for_state_dir_with_hook_timeout(
        state_dir: impl AsRef<Path>,
        hook_timeout: Duration,
    ) -> Self {
        Self {
            worktrees_root: state_dir.as_ref().join("worktrees"),
            hook_timeout,
        }
    }

    /// Parent directory of every per-session worktree this manager owns.
    pub fn root(&self) -> &Path {
        &self.worktrees_root
    }

    /// Resolve `repo_root`'s base branch using the brief §2.2 step `[1]`:
    ///
    /// 1. `git symbolic-ref refs/remotes/origin/HEAD` →
    ///    `refs/remotes/origin/main`
    /// 2. fall back to `git symbolic-ref HEAD` → `refs/heads/main`
    ///
    /// Returns `(base_branch, base_sha)`. `base_branch` is the short form
    /// (`main`); `base_sha` is the 40-char hex of that ref.
    pub async fn resolve_base_branch(&self, repo_root: &Path) -> CoreResult<(String, String)> {
        git::resolve_base_branch(repo_root).await
    }

    /// Create a fresh worktree at
    /// `<worktrees_root>/<project_slug>/<short_sid>` rooted at `base_sha`.
    ///
    /// Atomic on failure: returns a typed [`CoreError`] in the
    /// `-33110..-33119` range and leaves no branch or directory behind —
    /// the caller persists `sessions` *after* this returns `Ok`, never
    /// before. The branch name is fixed at `la/session-<short_sid>` so
    /// `git branch --list 'la/session-*'` is the operator escape hatch.
    pub async fn create(
        &self,
        repo_root: &Path,
        project_slug: &str,
        sid: &str,
        base: (String, String),
    ) -> CoreResult<WorktreePlan> {
        let short = short_sid(sid);
        let path = self.path_for(project_slug, &short);
        let branch = branch_name_for(&short);
        let (base_branch, base_sha) = base;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CoreError::WorktreeIo)?;
        }

        // Refuse to overwrite an existing slot — a leftover directory is
        // either a previous crashed run (operator should `git worktree
        // prune` + clean) or a hash collision (shouldn't happen with v7).
        if path.exists() {
            return Err(CoreError::WorktreeBusy {
                reason: format!("worktree path already exists: {}", path.display()),
            });
        }

        match git::worktree_add(repo_root, &path, &branch, &base_sha).await {
            Ok(()) => Ok(WorktreePlan {
                path,
                branch,
                base_branch,
                base_sha,
            }),
            Err(err) => {
                // Best-effort rollback — `git worktree add -b` may have
                // left a partial state (branch ref created before the
                // worktree directory in some failure modes). Run both
                // cleanups and ignore their errors so we surface the
                // original classified error to the caller.
                let _ = git::worktree_remove(repo_root, &path, true).await;
                let _ = git::branch_delete(repo_root, &branch, true).await;
                if path.exists() {
                    let _ = tokio::fs::remove_dir_all(&path).await;
                }
                Err(err)
            }
        }
    }

    /// Run the optional `<repo_root>/.lazyagents/hooks/post-create.sh`
    /// script inside the new worktree with a [`POST_CREATE_HOOK_TIMEOUT`]
    /// wall budget. **Never returns `Err`** — the brief makes hook failure
    /// advisory, so every error mode maps to a [`HookStatus`] variant the
    /// caller persists.
    pub async fn run_post_create_hook(
        &self,
        repo_root: &Path,
        plan: &WorktreePlan,
        adapter_kind: &str,
        session_id: &str,
    ) -> HookStatus {
        let hook_path = repo_root.join(HOOK_RELATIVE_PATH);
        if !hook_is_runnable(&hook_path).await {
            return HookStatus::Skipped;
        }

        run_hook(
            &hook_path,
            self.hook_timeout,
            HookContext {
                cwd: &plan.path,
                session_id,
                adapter_kind,
                branch: &plan.branch,
                base_branch: &plan.base_branch,
                base_sha: &plan.base_sha,
                repo_root,
            },
        )
        .await
    }

    /// Tear down a worktree according to `mode`. Idempotent — a
    /// worktree-already-gone result is `Ok(_)` so the dispatcher can
    /// retry without coordination. Branch deletion follows
    /// [`CleanupMode`]:
    ///
    /// - [`CleanupMode::Remove`]: drop the branch unconditionally.
    /// - [`CleanupMode::Force`]: drop the branch and pass `--force` to
    ///   `git worktree remove`.
    /// - [`CleanupMode::KeepBranchIfDirty`]: keep the branch if
    ///   `git rev-list base..branch` is non-empty (the agent committed
    ///   something the user may want to recover).
    ///
    /// Returns `Ok(true)` when the branch was **preserved** and
    /// `Ok(false)` when it was deleted — callers persist that bit so
    /// `sessions.worktree_branch` survives in the row whenever the
    /// branch is still on disk, per WEK-8 §2.4.
    pub async fn cleanup(&self, handle: &WorktreeHandle, mode: CleanupMode) -> CoreResult<bool> {
        let force = matches!(mode, CleanupMode::Force);

        // `git::worktree_remove` already swallows the "already gone"
        // case so the cleanup path is idempotent. WorktreeBusy on a
        // non-force call retries with --force — git locks the directory
        // when another process briefly holds it (notably IDEs scanning
        // .git), and the operator already asked for cleanup.
        match git::worktree_remove(&handle.repo_root, &handle.worktree_path, force).await {
            Ok(()) => {}
            Err(CoreError::WorktreeBusy { .. }) if !force => {
                git::worktree_remove(&handle.repo_root, &handle.worktree_path, true).await?;
            }
            Err(err) => return Err(err),
        }
        // Best-effort: the directory should be gone, but on edge cases
        // (NFS, mount points) git leaves it behind — remove the empty
        // shell so the next create can use the same name.
        if handle.worktree_path.exists() {
            let _ = tokio::fs::remove_dir_all(&handle.worktree_path).await;
        }

        let keep_branch = match mode {
            CleanupMode::KeepBranchIfDirty => {
                match git::branch_has_commits_beyond(
                    &handle.repo_root,
                    &handle.branch,
                    &handle.base_branch,
                )
                .await
                {
                    Ok(has_extra) => has_extra,
                    Err(err) => {
                        // Can't tell whether the branch carries
                        // un-merged commits — base ref renamed/deleted,
                        // git temporarily flaky, etc. The contract is
                        // "keep the branch when in doubt" so a user/
                        // agent's committed work isn't silently lost;
                        // operators can always `git branch -D` it later.
                        tracing::warn!(
                            repo = %handle.repo_root.display(),
                            branch = %handle.branch,
                            base = %handle.base_branch,
                            %err,
                            "branch_has_commits_beyond failed; preserving \
                             branch conservatively (KeepBranchIfDirty)"
                        );
                        true
                    }
                }
            }
            CleanupMode::Remove | CleanupMode::Force => false,
        };

        if !keep_branch {
            // `-D` (force) here matches git's expectation when the branch
            // isn't merged into HEAD — the common case for an agent's
            // session branch.
            git::branch_delete(&handle.repo_root, &handle.branch, true).await?;
        }
        Ok(keep_branch)
    }

    /// Best-effort `git worktree prune` against `repo_root`. Never panics
    /// and never returns `Err` — its only job is to reap orphan worktree
    /// entries left by crashed daemons before any RPC handler runs.
    pub async fn prune_orphans(&self, repo_root: &Path) {
        if let Err(err) = git::worktree_prune(repo_root).await {
            tracing::debug!(
                repo = %repo_root.display(),
                %err,
                "worktree prune failed (ignored)"
            );
        }
    }

    /// Reap worktree directories belonging to archived sessions whose
    /// `archived_at` is older than `ttl`. Defaults to the 7-day window
    /// pinned by the WEK-27 issue body.
    ///
    /// For each matched row:
    ///
    /// 1. Reconstruct a [`WorktreeHandle`] using the project root
    ///    looked up from `projects`.
    /// 2. Call [`cleanup`](Self::cleanup) with [`CleanupMode::Force`] —
    ///    archived rows are past the point where the user is editing
    ///    the worktree, so `--force` is safe and avoids leaking
    ///    directories git left locked on the previous attempt.
    /// 3. Clear the row's `worktree_path` / `worktree_branch` on
    ///    success so the next sweep skips it.
    ///
    /// Never returns `Err` — a single bad row should not abort the
    /// whole sweep. Returns `(swept_ok, swept_err)` so the daemon can
    /// log a one-liner. Designed to be called from
    /// [`Daemon::bind`](../../../la_daemon/runtime/struct.Daemon.html#method.bind)
    /// on startup and from a periodic tick thereafter.
    pub async fn sweep_expired(
        &self,
        storage: &la_storage::Storage,
        ttl: Duration,
    ) -> (usize, usize) {
        let ttl_seconds = ttl.as_secs().min(i64::MAX as u64) as i64;
        let rows = match storage
            .sessions()
            .list_archived_with_worktree_older_than_seconds(ttl_seconds)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "sweep_expired: list query failed");
                return (0, 0);
            }
        };
        if rows.is_empty() {
            return (0, 0);
        }

        let mut ok = 0usize;
        let mut err_count = 0usize;
        for row in rows {
            let (Some(wt_path), Some(branch), Some(base_branch)) = (
                row.worktree_path.as_deref(),
                row.worktree_branch.as_deref(),
                row.base_branch.as_deref(),
            ) else {
                // Row was filtered for worktree_path NOT NULL above, so
                // a missing branch/base column is a schema drift that
                // we can't reconstruct a handle from. Clear the path so
                // the next sweep doesn't pick it up forever.
                let _ = storage.sessions().clear_worktree(&row.id, false).await;
                continue;
            };
            let repo_root = match storage.projects().get(&row.project_id).await {
                Ok(Some(p)) => PathBuf::from(p.root_path),
                _ => {
                    // Project gone — clear the row so future sweeps
                    // don't keep retrying a path we can't drive git
                    // against. The on-disk directory becomes pure
                    // garbage that `git worktree prune` won't see, but
                    // that's an operator-cleanup edge case.
                    let _ = storage.sessions().clear_worktree(&row.id, false).await;
                    continue;
                }
            };
            let handle = Self::handle_from_row(repo_root, wt_path, branch, base_branch);
            match self.cleanup(&handle, CleanupMode::Force).await {
                Ok(_kept_branch) => {
                    // `Force` always drops the branch (cleanup returns
                    // `false`), so `keep_branch=false` is the correct
                    // bit to persist regardless of the return value.
                    let _ = storage.sessions().clear_worktree(&row.id, false).await;
                    ok += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        session = %row.id,
                        path = %handle.worktree_path.display(),
                        %e,
                        "sweep_expired: cleanup failed; row kept for next pass"
                    );
                    err_count += 1;
                }
            }
        }
        (ok, err_count)
    }

    /// Compute the canonical worktree path for `(project_slug,
    /// short_sid)` without touching the filesystem. Public so callers can
    /// describe the intended path in pre-flight diagnostics or tests.
    pub fn path_for(&self, project_slug: &str, short_sid: &str) -> PathBuf {
        self.worktrees_root.join(project_slug).join(short_sid)
    }

    /// Reconstruct a [`WorktreeHandle`] from a stored session row's three
    /// worktree columns plus its project root. Callers should only invoke
    /// this when all three columns are `Some` — see
    /// `SessionsRepo::list_with_worktree_by_project`.
    pub fn handle_from_row(
        repo_root: PathBuf,
        worktree_path: &str,
        worktree_branch: &str,
        base_branch: &str,
    ) -> WorktreeHandle {
        WorktreeHandle {
            repo_root,
            worktree_path: PathBuf::from(worktree_path),
            branch: worktree_branch.to_string(),
            base_branch: base_branch.to_string(),
        }
    }
}

struct HookContext<'a> {
    cwd: &'a Path,
    session_id: &'a str,
    adapter_kind: &'a str,
    branch: &'a str,
    base_branch: &'a str,
    base_sha: &'a str,
    repo_root: &'a Path,
}

async fn run_hook(hook_path: &Path, hook_timeout: Duration, ctx: HookContext<'_>) -> HookStatus {
    use tokio::process::Command;

    let mut cmd = Command::new(hook_path);
    cmd.current_dir(ctx.cwd);
    cmd.env("LA_SESSION_ID", ctx.session_id);
    cmd.env("LA_ADAPTER_KIND", ctx.adapter_kind);
    cmd.env("LA_BRANCH", ctx.branch);
    cmd.env("LA_BASE_BRANCH", ctx.base_branch);
    cmd.env("LA_BASE_SHA", ctx.base_sha);
    cmd.env("LA_PROJECT_ROOT", ctx.repo_root);
    cmd.env("LA_WORKTREE_PATH", ctx.cwd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(
                hook = %hook_path.display(),
                %err,
                "post-create hook spawn failed; status=failed"
            );
            return HookStatus::Failed;
        }
    };

    match tokio::time::timeout(hook_timeout, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            tracing::debug!(hook = %hook_path.display(), "post-create hook ok");
            HookStatus::Ok
        }
        Ok(Ok(output)) => {
            tracing::warn!(
                hook = %hook_path.display(),
                code = output.status.code().unwrap_or(-1),
                stderr = %truncate_for_log(&output.stderr),
                "post-create hook returned non-zero; status=failed"
            );
            HookStatus::Failed
        }
        Ok(Err(err)) => {
            tracing::warn!(
                hook = %hook_path.display(),
                %err,
                "post-create hook wait failed; status=failed"
            );
            HookStatus::Failed
        }
        Err(_elapsed) => {
            tracing::warn!(
                hook = %hook_path.display(),
                budget_secs = hook_timeout.as_secs(),
                "post-create hook timed out; status=timeout"
            );
            HookStatus::Timeout
        }
    }
}

fn truncate_for_log(bytes: &[u8]) -> String {
    const CAP: usize = 1024;
    if bytes.len() <= CAP {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let mut s = String::from_utf8_lossy(&bytes[..CAP]).into_owned();
        s.push_str("…[truncated]");
        s
    }
}

#[cfg(unix)]
async fn hook_is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() => meta.permissions().mode() & 0o111 != 0,
        _ => false,
    }
}

#[cfg(not(unix))]
async fn hook_is_runnable(path: &Path) -> bool {
    // WEK-27 ships Linux-only; on Windows we just check file existence
    // and let the OS reject non-executable invocations later. A proper
    // .bat / .cmd lookup is follow-up work.
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Slug-safe basename for a repo root, suffixed with the first 8 hex
/// chars of SHA-256(abs_path) so two `~/code/api/` checkouts don't
/// collide. Result is `[A-Za-z0-9_.-]+-[0-9a-f]{8}`.
pub fn project_slug(repo_root: &Path) -> String {
    let abs = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let basename = abs
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let cleaned: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = if cleaned.is_empty() {
        "repo".to_string()
    } else {
        cleaned
    };

    let mut hasher = Sha256::new();
    hasher.update(abs.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    let suffix: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{cleaned}-{suffix}")
}

/// Short, branch-safe session id. Keep the v7 timestamp prefix for roughly
/// sortable directories, but include the random tail so same-tick concurrent
/// sessions do not collide on worktree path or `la/session-*` branch name.
pub fn short_sid(sid: &str) -> String {
    let hex: String = sid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() <= 20 {
        return hex;
    }
    format!("{}{}", &hex[..12], &hex[hex.len() - 8..])
}

/// Branch naming convention pinned by the brief: `la/session-<short_sid>`.
pub fn branch_name_for(short_sid: &str) -> String {
    format!("la/session-{short_sid}")
}
