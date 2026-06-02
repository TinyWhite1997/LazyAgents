//! Thin wrappers around `git` subprocess invocations used by
//! [`super::manager::WorktreeManager`].
//!
//! Every `git` invocation in `la-core` goes through this module. Two
//! reasons:
//!
//! 1. **Single classification point.** `git worktree add` failures map
//!    to one of six [`crate::CoreError`] variants via
//!    [`classify_add_error`]. New stderr patterns only need a branch
//!    here; the manager never matches on raw strings.
//! 2. **Single concurrency model.** All commands are
//!    [`tokio::process::Command`] children with a 30 s wall cap, so a
//!    wedged `git index.lock` can't take the session-spawn path with
//!    it.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::error::{CoreError, CoreResult};

/// Hard upper bound for any single git subprocess we issue. Keeps a
/// wedged repo from blocking `sessions.create` indefinitely.
const GIT_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum bytes of git stderr we surface to the user when classifying
/// generic provisioning failures. Matches the contract documented on
/// [`crate::CoreError::WorktreeProvision`].
pub(crate) const MAX_STDERR_BYTES: usize = 4 * 1024;

pub(crate) struct GitOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    /// Raw stdout bytes — preserved because the diff parser slices the
    /// hunk body out of the *raw* `git diff` stream rather than
    /// re-stringifying parsed lines.
    pub stdout_bytes: Vec<u8>,
    /// Exit status code, if the child terminated normally. `None` when
    /// the process was killed by a signal.
    pub exit_code: Option<i32>,
}

/// Run `git -C <repo_root> <args>` with the standard 30 s timeout.
/// Returns the combined output; the caller decides what to do with
/// non-zero exit. Translates "git binary missing" into a typed
/// [`CoreError::GitUnavailable`] so the user sees an actionable hint.
pub(crate) async fn run_git(repo_root: &Path, args: &[&str]) -> CoreResult<GitOutput> {
    run_git_with_stdin(repo_root, args, None).await
}

/// Same as [`run_git`] but writes `stdin_bytes` to the child's stdin
/// before reading output. Used by `git apply --cached -` and
/// `git commit -F -` so we never have to shell-escape patch text or
/// commit messages.
pub(crate) async fn run_git_with_stdin(
    repo_root: &Path,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
) -> CoreResult<GitOutput> {
    use tokio::io::AsyncWriteExt;

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_root);
    for a in args {
        cmd.arg(a);
    }
    // Force English git messages so `classify_add_error` doesn't have
    // to know every locale's translation of "already exists".
    cmd.env("LC_ALL", "C");
    cmd.env("LANG", "C");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if stdin_bytes.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }

    let spawn = match cmd.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CoreError::GitUnavailable {
                hint: "the `git` binary was not found on $PATH".to_string(),
            });
        }
        Err(err) => return Err(CoreError::WorktreeIo(err)),
    };
    let mut child = spawn;
    if let (Some(bytes), Some(mut stdin)) = (stdin_bytes, child.stdin.take()) {
        if let Err(err) = stdin.write_all(bytes).await {
            return Err(CoreError::WorktreeIo(err));
        }
        drop(stdin);
    }

    let output_future = child.wait_with_output();
    let output = match timeout(GIT_CMD_TIMEOUT, output_future).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => return Err(CoreError::WorktreeIo(err)),
        Err(_elapsed) => {
            return Err(CoreError::WorktreeProvision {
                stderr: format!("git {} timed out after {GIT_CMD_TIMEOUT:?}", args.join(" ")),
            });
        }
    };

    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout_bytes: output.stdout,
    })
}

/// Resolve a project's default base branch and its tip SHA.
///
/// Tries, in order:
///
/// 1. `git symbolic-ref refs/remotes/origin/HEAD` — works whenever the
///    repo has been `git clone`d from a remote that publishes HEAD.
/// 2. `git symbolic-ref HEAD` — fallback for bare local branches.
///
/// Returns `(short_branch_name, sha)`. The branch name is the short
/// form (e.g. `main`, not `refs/heads/main`) so it survives a round
/// trip into SQLite + the TUI badge.
pub(super) async fn resolve_base_branch(repo_root: &Path) -> CoreResult<(String, String)> {
    let origin = run_git(repo_root, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await?;
    let full_ref = if origin.success {
        origin.stdout.trim().to_string()
    } else {
        // Fall back to local HEAD. If that *also* fails it's almost
        // always because the path isn't a git repo at all — re-probe so
        // we can return the right typed error instead of opaque
        // provision failure stderr.
        let local = run_git(repo_root, &["symbolic-ref", "HEAD"]).await?;
        if !local.success {
            let probe = run_git(repo_root, &["rev-parse", "--git-dir"]).await?;
            if !probe.success {
                return Err(CoreError::NotAGitRepo {
                    path: repo_root.display().to_string(),
                });
            }
            return Err(CoreError::WorktreeProvision {
                stderr: trim_stderr(&local.stderr),
            });
        }
        local.stdout.trim().to_string()
    };

    let sha_out = run_git(repo_root, &["rev-parse", &full_ref]).await?;
    if !sha_out.success {
        return Err(CoreError::WorktreeProvision {
            stderr: trim_stderr(&sha_out.stderr),
        });
    }

    Ok((
        short_branch_name(&full_ref),
        sha_out.stdout.trim().to_string(),
    ))
}

/// Strip `refs/heads/` or `refs/remotes/<remote>/` prefix from a full
/// ref so the TUI can show `main` instead of `refs/remotes/origin/main`.
fn short_branch_name(full_ref: &str) -> String {
    if let Some(rest) = full_ref.strip_prefix("refs/heads/") {
        return rest.to_string();
    }
    if let Some(rest) = full_ref.strip_prefix("refs/remotes/") {
        // refs/remotes/<remote>/<branch>
        if let Some((_, branch)) = rest.split_once('/') {
            return branch.to_string();
        }
        return rest.to_string();
    }
    full_ref.to_string()
}

/// Provision a new worktree.
///
/// Issues a single `git worktree add <wt_path> -b <branch> <base_sha>`
/// — atomic by `git`'s own contract: branch creation + checkout happen
/// together. Failures funnel through [`classify_add_error`] so the
/// caller gets a typed [`CoreError`] instead of raw stderr.
pub(super) async fn worktree_add(
    repo_root: &Path,
    wt_path: &Path,
    branch: &str,
    base_sha: &str,
) -> CoreResult<()> {
    let wt_str = wt_path.to_string_lossy().into_owned();
    let out = run_git(
        repo_root,
        &["worktree", "add", &wt_str, "-b", branch, base_sha],
    )
    .await?;
    if out.success {
        Ok(())
    } else {
        Err(classify_add_error(&out.stderr))
    }
}

/// Tear down a worktree directory recorded under
/// `<repo>/.git/worktrees/<name>`. `force = true` passes `--force`,
/// which `cleanup(_, Force)` and the create-rollback path both rely on
/// to recover from half-failed state.
///
/// "Already gone" is treated as success — both
/// [`super::manager::WorktreeManager::cleanup`] and the
/// `WorktreeManager::create` rollback are idempotent by contract.
pub(super) async fn worktree_remove(
    repo_root: &Path,
    wt_path: &Path,
    force: bool,
) -> CoreResult<()> {
    let wt_str = wt_path.to_string_lossy().into_owned();
    let args: Vec<&str> = if force {
        vec!["worktree", "remove", "--force", &wt_str]
    } else {
        vec!["worktree", "remove", &wt_str]
    };
    let out = run_git(repo_root, &args).await?;
    if out.success {
        return Ok(());
    }
    if stderr_means_already_gone(&out.stderr) {
        return Ok(());
    }
    if stderr_means_busy(&out.stderr) {
        return Err(CoreError::WorktreeBusy {
            reason: trim_stderr(&out.stderr),
        });
    }
    Err(CoreError::WorktreeProvision {
        stderr: trim_stderr(&out.stderr),
    })
}

/// Delete a local branch. `force = true` ⇒ `-D`, otherwise `-d` (which
/// refuses non-merged branches). Idempotent on "branch not found".
pub(super) async fn branch_delete(repo_root: &Path, branch: &str, force: bool) -> CoreResult<()> {
    let flag = if force { "-D" } else { "-d" };
    let out = run_git(repo_root, &["branch", flag, branch]).await?;
    if out.success {
        return Ok(());
    }
    if out.stderr.contains("not found")
        || out.stderr.contains("no branch named")
        || out.stderr.contains("not a valid")
    {
        return Ok(());
    }
    Err(CoreError::WorktreeProvision {
        stderr: trim_stderr(&out.stderr),
    })
}

/// `true` if `branch` carries any commits not reachable from
/// `base_branch`. Used by `cleanup(_, KeepBranchIfDirty)` to decide
/// whether the branch is safe to drop. Returns the typed `Err` on git
/// failure so the caller can choose its own conservative default —
/// `cleanup` treats `Err` as "keep the branch" so a transient git
/// failure (renamed/deleted base ref, locked repo) never erases work
/// the user might still want to recover.
pub(super) async fn branch_has_commits_beyond(
    repo_root: &Path,
    branch: &str,
    base_branch: &str,
) -> CoreResult<bool> {
    let range = format!("{base_branch}..{branch}");
    let out = run_git(repo_root, &["rev-list", "--count", &range]).await?;
    if !out.success {
        return Err(CoreError::WorktreeProvision {
            stderr: trim_stderr(&out.stderr),
        });
    }
    Ok(out
        .stdout
        .trim()
        .parse::<u64>()
        .map(|n| n > 0)
        .unwrap_or(false))
}

/// Best-effort `git worktree prune`. Caller logs and moves on — this
/// is daemon-startup hygiene, not a fault path.
pub(super) async fn worktree_prune(repo_root: &Path) -> CoreResult<()> {
    let out = run_git(repo_root, &["worktree", "prune"]).await?;
    if out.success {
        Ok(())
    } else {
        Err(CoreError::WorktreeProvision {
            stderr: trim_stderr(&out.stderr),
        })
    }
}

/// Map the stderr of a failed `git worktree add` to a typed
/// [`CoreError`]. Pattern matching is on the *English* phrases that
/// `git --version >= 2.20` emits — `LC_ALL=C` in [`run_git`] keeps
/// that assumption sound.
///
/// **This is the single sanctioned classifier.** New stderr patterns
/// belong here and nowhere else, per the M2 brief amendment R3.
pub fn classify_add_error(stderr: &str) -> CoreError {
    let s = stderr.to_ascii_lowercase();
    if s.contains("not a git repository")
        || s.contains("is not a working tree")
        || s.contains("is outside repository")
    {
        return CoreError::NotAGitRepo {
            path: trim_stderr(stderr),
        };
    }
    if s.contains("already exists") && s.contains("branch") {
        let branch = extract_quoted(stderr).unwrap_or_else(|| "<unknown>".to_string());
        return CoreError::BranchCollision { branch };
    }
    if stderr_means_busy(stderr) {
        return CoreError::WorktreeBusy {
            reason: trim_stderr(stderr),
        };
    }
    if s.contains("permission denied")
        || s.contains("no space left")
        || s.contains("unable to create")
        || s.contains("read-only file system")
    {
        return CoreError::WorktreeIo(std::io::Error::other(trim_stderr(stderr)));
    }
    CoreError::WorktreeProvision {
        stderr: trim_stderr(stderr),
    }
}

fn stderr_means_busy(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("is already checked out at") || s.contains("index.lock")
}

fn stderr_means_already_gone(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("is not a working tree")
        || s.contains("not a working tree")
        || s.contains("does not exist")
}

pub(crate) fn trim_stderr(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.len() <= MAX_STDERR_BYTES {
        trimmed.to_string()
    } else {
        let mut end = MAX_STDERR_BYTES;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

/// `"fatal: a branch named 'la/session-…' already exists"` →
/// `Some("la/session-…")`. Returns `None` for stderr without quoted
/// segments.
fn extract_quoted(s: &str) -> Option<String> {
    let mut iter = s.split('\'');
    iter.next()?;
    let inside = iter.next()?;
    if inside.is_empty() {
        None
    } else {
        Some(inside.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_branch_collision() {
        let stderr = "fatal: a branch named 'la/session-deadbeef' already exists\n";
        match classify_add_error(stderr) {
            CoreError::BranchCollision { branch } => {
                assert_eq!(branch, "la/session-deadbeef");
            }
            other => panic!("expected BranchCollision, got {other:?}"),
        }
    }

    #[test]
    fn classify_not_a_git_repo() {
        let stderr = "fatal: not a git repository (or any of the parent directories): .git\n";
        assert!(matches!(
            classify_add_error(stderr),
            CoreError::NotAGitRepo { .. }
        ));
    }

    #[test]
    fn classify_busy_index_lock() {
        let stderr = "fatal: Unable to create '/repo/.git/index.lock': File exists.\n";
        assert!(matches!(
            classify_add_error(stderr),
            CoreError::WorktreeBusy { .. }
        ));
    }

    #[test]
    fn classify_already_checked_out() {
        let stderr = "fatal: 'la/session-foo' is already checked out at '/tmp/wt-foo'\n";
        assert!(matches!(
            classify_add_error(stderr),
            CoreError::WorktreeBusy { .. }
        ));
    }

    #[test]
    fn classify_io_permission_denied() {
        let stderr = "fatal: unable to create '/root/wt': Permission denied\n";
        assert!(matches!(
            classify_add_error(stderr),
            CoreError::WorktreeIo(_)
        ));
    }

    #[test]
    fn classify_unknown_falls_back() {
        let stderr = "fatal: a thing we have never seen before\n";
        assert!(matches!(
            classify_add_error(stderr),
            CoreError::WorktreeProvision { .. }
        ));
    }

    #[test]
    fn trim_stderr_caps_at_4k() {
        let big = "x".repeat(MAX_STDERR_BYTES * 2);
        let trimmed = trim_stderr(&big);
        assert!(trimmed.ends_with('…'));
        assert!(trimmed.len() <= MAX_STDERR_BYTES + 4);
    }

    #[test]
    fn short_branch_name_strips_prefixes() {
        assert_eq!(short_branch_name("refs/heads/main"), "main");
        assert_eq!(short_branch_name("refs/remotes/origin/main"), "main");
        assert_eq!(short_branch_name("refs/remotes/upstream/feat/x"), "feat/x");
        assert_eq!(short_branch_name("main"), "main");
    }
}
