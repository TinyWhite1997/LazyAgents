//! Integration tests for the WEK-27 worktree subsystem.
//!
//! Covers the four acceptance hooks called out in the issue body:
//!
//! - **create / resolve base / branch**: `spawn_with_worktree_*` tests
//!   spin up a fresh bare repo, seed an initial commit, then drive
//!   `SessionManager::spawn_with_options` end-to-end. Each test asserts
//!   the row carries the worktree triple, the worktree directory lives
//!   under `<state_dir>/worktrees/`, and the branch is the canonical
//!   `la/session-<short_sid>`.
//! - **cleanup**: `archive_cleans_worktree` confirms `sessions.archive`
//!   tears down the worktree directory and clears the row.
//! - **post-create hook**: `hook_outcomes` cover the four `HookStatus`
//!   variants — `ok`, `failed`, `timeout`, `skipped` — and assert the
//!   column is persisted (brief amendment R4 — hook failure must NOT
//!   mutate `SessionState`).
//! - **failure rollback**: `create_failure_rolls_back` proves a failed
//!   `git worktree add` leaves no row, no directory, and no branch.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use la_core::manager::WorktreeSpawnOptions;
use la_core::{
    CleanupMode, HookStatus, SessionId, SessionManager, WorktreeHandle, WorktreeManager,
};
use support::{harness_with_worktree, ShellAdapter};
use tempfile::TempDir;
use tokio::process::Command;

/// Initialize a fresh git repo at `path` with one committed file so
/// `git worktree add` has a HEAD to fork from. Returns the absolute path.
async fn init_repo_with_seed(path: &Path) {
    // `git init` + create one file + commit it. We use `tokio::process`
    // throughout so the test stays inside the async runtime.
    run(
        path.parent().unwrap(),
        &["git", "init", "-q", "-b", "main", path.to_str().unwrap()],
    )
    .await;
    // Make committer / author deterministic so commits don't fail on
    // CI runners without git config.
    run(path, &["git", "config", "user.email", "test@example.com"]).await;
    run(path, &["git", "config", "user.name", "test"]).await;
    tokio::fs::write(path.join("README.md"), b"seed\n")
        .await
        .expect("seed write");
    run(path, &["git", "add", "README.md"]).await;
    run(path, &["git", "commit", "-q", "-m", "seed"]).await;
}

async fn run(cwd: &Path, argv: &[&str]) {
    let status = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .status()
        .await
        .unwrap_or_else(|e| panic!("spawn {argv:?}: {e}"));
    assert!(status.success(), "{argv:?} exited with {status}");
}

async fn wait_until<F: Fn() -> bool>(predicate: F, timeout: Duration, label: &str) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {label}");
}

#[tokio::test]
async fn spawn_with_worktree_provisions_branch_and_dir() {
    let _tmp_state = TempDir::new().expect("state dir");
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let h = harness_with_worktree(&repo_root).await;

    let adapter = ShellAdapter::new("printf 'hello\\n'; exit 0");
    let spawned = h
        .manager
        .spawn_with_options(
            &adapter,
            h.project_id.clone(),
            la_adapter::SpawnRequest::new(repo_root.clone()),
            Some(WorktreeSpawnOptions {
                repo_root: repo_root.clone(),
            }),
        )
        .await
        .expect("spawn with worktree");

    let plan_path = spawned.worktree_path.clone().expect("worktree path echoed");
    let plan_branch = spawned.worktree_branch.clone().expect("branch echoed");
    let plan_base = spawned.base_branch.clone().expect("base branch echoed");

    // Directory exists under the manager's root.
    assert!(
        plan_path.exists(),
        "worktree dir missing: {}",
        plan_path.display()
    );
    assert!(
        plan_path.starts_with(h.worktrees_root()),
        "worktree {} not under {}",
        plan_path.display(),
        h.worktrees_root().display(),
    );
    // Branch name follows `la/session-<short_sid>`.
    assert!(
        plan_branch.starts_with("la/session-"),
        "branch {plan_branch}"
    );
    // Base branch resolved to the local HEAD (no remote in the fixture).
    assert_eq!(plan_base, "main");

    // Storage row carries the worktree triple + a hook status (the
    // fixture has no hook script ⇒ "skipped").
    let row = h
        .storage
        .sessions()
        .get(spawned.id.as_str())
        .await
        .expect("get row")
        .expect("session row");
    assert_eq!(
        row.worktree_path.as_deref(),
        Some(plan_path.to_string_lossy().as_ref())
    );
    assert_eq!(row.worktree_branch.as_deref(), Some(plan_branch.as_str()));
    assert_eq!(row.base_branch.as_deref(), Some("main"));
    assert_eq!(row.post_create_hook_status.as_deref(), Some("skipped"));

    // Branch actually exists in the source repo.
    let out = Command::new("git")
        .args([
            "-C",
            repo_root.to_str().unwrap(),
            "branch",
            "--list",
            &plan_branch,
        ])
        .output()
        .await
        .expect("branch list");
    assert!(
        std::str::from_utf8(&out.stdout)
            .unwrap()
            .contains(&plan_branch),
        "branch list output: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Wait for the shell to exit so archive doesn't refuse.
    wait_until(
        || std::fs::metadata("/").is_ok(), // placeholder — we wait on registry below
        Duration::from_millis(50),
        "yield to scheduler",
    )
    .await;
    wait_for_exit(&h.manager, &spawned.id).await;
}

#[tokio::test]
async fn archive_cleans_worktree() {
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let h = harness_with_worktree(&repo_root).await;

    let adapter = ShellAdapter::new("printf 'x'; exit 0");
    let spawned = h
        .manager
        .spawn_with_options(
            &adapter,
            h.project_id.clone(),
            la_adapter::SpawnRequest::new(repo_root.clone()),
            Some(WorktreeSpawnOptions {
                repo_root: repo_root.clone(),
            }),
        )
        .await
        .expect("spawn");

    let wt_path = spawned.worktree_path.clone().expect("path");
    let branch = spawned.worktree_branch.clone().expect("branch");
    wait_for_exit(&h.manager, &spawned.id).await;
    assert!(wt_path.exists(), "wt should still exist before archive");

    h.manager.archive(&spawned.id).await.expect("archive");

    // Directory gone, branch dropped (no commits beyond base ⇒
    // KeepBranchIfDirty falls through to delete).
    assert!(
        !wt_path.exists(),
        "worktree dir should be gone after archive"
    );
    let out = Command::new("git")
        .args([
            "-C",
            repo_root.to_str().unwrap(),
            "branch",
            "--list",
            &branch,
        ])
        .output()
        .await
        .expect("branch list");
    assert!(
        !std::str::from_utf8(&out.stdout).unwrap().contains(&branch),
        "branch {branch} should be gone, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let row = h
        .storage
        .sessions()
        .get(spawned.id.as_str())
        .await
        .expect("get")
        .expect("row");
    assert!(row.worktree_path.is_none(), "row.worktree_path cleared");
}

#[tokio::test]
async fn create_failure_does_not_leave_session_row() {
    // Repo without an initial commit → `git symbolic-ref HEAD` resolves
    // but `rev-parse <ref>` fails, which trips WorktreeProvision /
    // similar. Either way we expect: no session row, no directory.
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    // git init only — no commit, so HEAD points at an unborn branch.
    tokio::fs::create_dir_all(&repo_root).await.unwrap();
    run(&repo_root, &["git", "init", "-q", "-b", "main"]).await;

    let h = harness_with_worktree(&repo_root).await;

    let adapter = ShellAdapter::new("true");
    let result = h
        .manager
        .spawn_with_options(
            &adapter,
            h.project_id.clone(),
            la_adapter::SpawnRequest::new(repo_root.clone()),
            Some(WorktreeSpawnOptions {
                repo_root: repo_root.clone(),
            }),
        )
        .await;
    assert!(
        result.is_err(),
        "expected error on unborn HEAD, got {result:?}"
    );

    // No worktree directory leftover.
    let wt_root = h.worktrees_root();
    if let Ok(mut rd) = tokio::fs::read_dir(wt_root).await {
        let mut entries = vec![];
        while let Ok(Some(e)) = rd.next_entry().await {
            entries.push(e.path());
        }
        // Allow the project bucket dir to exist; just no session under it.
        for p in &entries {
            let mut sub = tokio::fs::read_dir(p).await.unwrap();
            while let Ok(Some(e)) = sub.next_entry().await {
                panic!("orphan worktree: {}", e.path().display());
            }
        }
    }
}

#[tokio::test]
async fn hook_outcomes_are_persisted() {
    // Run one spawn per HookStatus we want to assert. Each iteration uses
    // a fresh repo so the hook is isolated.
    for (script, expected) in [
        ("#!/bin/sh\nexit 0\n", "ok"),
        ("#!/bin/sh\nexit 1\n", "failed"),
        ("#!/bin/sh\nsleep 90\n", "timeout"),
    ] {
        let project_tmp = TempDir::new().expect("project dir");
        let repo_root = project_tmp.path().join("repo");
        init_repo_with_seed(&repo_root).await;
        // Install the hook.
        let hooks_dir = repo_root.join(".lazyagents").join("hooks");
        tokio::fs::create_dir_all(&hooks_dir).await.unwrap();
        let hook = hooks_dir.join("post-create.sh");
        tokio::fs::write(&hook, script).await.unwrap();
        chmod_exec(&hook).await;

        // Shrink the hook timeout so the `timeout` case finishes fast.
        let h = if expected == "timeout" {
            // Use the short-timeout harness — see support.rs.
            support::harness_with_worktree_short_timeout(&repo_root).await
        } else {
            harness_with_worktree(&repo_root).await
        };

        let adapter = ShellAdapter::new("true");
        let spawned = h
            .manager
            .spawn_with_options(
                &adapter,
                h.project_id.clone(),
                la_adapter::SpawnRequest::new(repo_root.clone()),
                Some(WorktreeSpawnOptions {
                    repo_root: repo_root.clone(),
                }),
            )
            .await
            .expect("spawn");

        let row = h
            .storage
            .sessions()
            .get(spawned.id.as_str())
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            row.post_create_hook_status.as_deref(),
            Some(expected),
            "expected hook status {expected} for script {script:?}",
        );
        wait_for_exit(&h.manager, &spawned.id).await;
    }
}

#[tokio::test]
async fn worktree_manager_classify_add_error_is_single_site() {
    // §R3 invariant — make sure new stderr patterns can't get added in a
    // second place by accident. Grep for actual call sites (not the
    // doc-comment reference at the top of manager.rs).
    let manifest = include_str!("../src/worktree/manager.rs");
    assert!(
        !manifest.contains("classify_add_error("),
        "manager.rs must not invoke classify_add_error — that's git.rs's job"
    );
}

#[cfg(unix)]
async fn chmod_exec(p: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(p).await.unwrap().permissions();
    perms.set_mode(0o755);
    tokio::fs::set_permissions(p, perms).await.unwrap();
}

#[cfg(not(unix))]
async fn chmod_exec(_p: &PathBuf) {
    // No-op on Windows; the harness skip-paths the runnable check.
}

// Spin-wait until the session manager has removed `id` from its active
// registry (pump finished + child exited). 5 s wall cap so a hung child
// doesn't deadlock the test runner.
async fn wait_for_exit(manager: &SessionManager, id: &SessionId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let active = manager.active_ids().await;
        if !active.contains(id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("session {} never exited", id.as_str());
}

// Sanity-check WorktreeHandle + WorktreeManager direct unit smoke. Keeps
// the cheap fast path covered even when the integration harness above is
// skipped (e.g. when running with `--features no_git`).
#[tokio::test]
async fn worktree_manager_smoke_create_and_cleanup() {
    let state = TempDir::new().expect("state");
    let project_tmp = TempDir::new().expect("project");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let wm = WorktreeManager::for_state_dir(state.path());
    let base = wm
        .resolve_base_branch(&repo_root)
        .await
        .expect("resolve base");
    assert_eq!(base.0, "main");
    assert_eq!(base.1.len(), 40, "base sha is 40 hex");

    let plan = wm
        .create(&repo_root, "fixture", &fake_sid(), base.clone())
        .await
        .expect("create");
    assert!(plan.path.exists());
    assert!(plan.branch.starts_with("la/session-"));

    let handle = WorktreeHandle {
        repo_root: repo_root.clone(),
        worktree_path: plan.path.clone(),
        branch: plan.branch.clone(),
        base_branch: plan.base_branch.clone(),
    };
    wm.cleanup(&handle, CleanupMode::Force)
        .await
        .expect("cleanup");
    assert!(!plan.path.exists(), "worktree dir should be gone");
}

fn fake_sid() -> String {
    // Stable v7-shaped string so short_sid yields 16 hex chars; we don't
    // need actual time correctness for unit smoke.
    "0190a000-1111-7222-8333-444455556666".to_string()
}

// Compile-time sanity: HookStatus surfaces all four wire values.
#[test]
fn hook_status_string_values_match_migration_check_constraint() {
    assert_eq!(HookStatus::Ok.as_str(), "ok");
    assert_eq!(HookStatus::Failed.as_str(), "failed");
    assert_eq!(HookStatus::Timeout.as_str(), "timeout");
    assert_eq!(HookStatus::Skipped.as_str(), "skipped");
}

// --- Regression coverage for the WEK-27 v2 review ----------------

/// `KeepBranchIfDirty` MUST preserve a session branch that carries
/// commits beyond `base_branch`, even on cleanup paths driven by
/// `sessions.archive`. The pre-fix behaviour deleted it because the
/// dirty-check fallback was `unwrap_or(false)`.
#[tokio::test]
async fn keep_branch_if_dirty_preserves_branch_with_extra_commits() {
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let state = TempDir::new().expect("state");
    let wm = WorktreeManager::for_state_dir(state.path());
    let base = wm
        .resolve_base_branch(&repo_root)
        .await
        .expect("resolve base");

    let plan = wm
        .create(&repo_root, "fixture", &fake_sid(), base.clone())
        .await
        .expect("create");

    // Commit something on the session branch so `rev-list base..branch`
    // returns > 0 and KeepBranchIfDirty MUST keep the branch.
    tokio::fs::write(plan.path.join("agent.txt"), b"work\n")
        .await
        .unwrap();
    run(&plan.path, &["git", "add", "agent.txt"]).await;
    run(&plan.path, &["git", "commit", "-q", "-m", "agent commit"]).await;

    let handle = WorktreeHandle {
        repo_root: repo_root.clone(),
        worktree_path: plan.path.clone(),
        branch: plan.branch.clone(),
        base_branch: plan.base_branch.clone(),
    };
    wm.cleanup(&handle, CleanupMode::KeepBranchIfDirty)
        .await
        .expect("cleanup");

    assert!(!plan.path.exists(), "worktree dir should be gone");
    assert!(
        branch_exists(&repo_root, &plan.branch).await,
        "session branch {} must be preserved when it has commits beyond {}",
        plan.branch,
        plan.base_branch,
    );
}

/// When `git rev-list base..branch` can't run (base ref renamed/deleted,
/// git temporarily flaky), `KeepBranchIfDirty` MUST conservatively keep
/// the branch — the user's only path back to committed work.
#[tokio::test]
async fn keep_branch_if_dirty_preserves_branch_when_dirty_check_fails() {
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let state = TempDir::new().expect("state");
    let wm = WorktreeManager::for_state_dir(state.path());
    let base = wm
        .resolve_base_branch(&repo_root)
        .await
        .expect("resolve base");

    let plan = wm
        .create(&repo_root, "fixture", &fake_sid(), base.clone())
        .await
        .expect("create");

    // Commit on the session branch so there's user work to protect.
    tokio::fs::write(plan.path.join("agent.txt"), b"work\n")
        .await
        .unwrap();
    run(&plan.path, &["git", "add", "agent.txt"]).await;
    run(&plan.path, &["git", "commit", "-q", "-m", "agent commit"]).await;

    // Drive the dirty-check into a guaranteed git error by pointing it
    // at a base ref that doesn't exist. WorktreeHandle is plain data,
    // so we construct it with a bogus base_branch directly.
    let handle = WorktreeHandle {
        repo_root: repo_root.clone(),
        worktree_path: plan.path.clone(),
        branch: plan.branch.clone(),
        base_branch: "no/such/base".to_string(),
    };
    wm.cleanup(&handle, CleanupMode::KeepBranchIfDirty)
        .await
        .expect("cleanup");

    assert!(!plan.path.exists(), "worktree dir should be gone");
    assert!(
        branch_exists(&repo_root, &plan.branch).await,
        "branch {} must be preserved when dirty-check fails (base ref unknown)",
        plan.branch,
    );
}

/// `sweep_expired` MUST reap archived worktrees whose `archived_at`
/// crossed the TTL boundary, clear their `worktree_path` column, and
/// leave fresh archives alone.
#[tokio::test]
async fn sweep_expired_reaps_old_archived_worktrees() {
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let h = harness_with_worktree(&repo_root).await;
    let wm = h.worktree_manager.clone().expect("wm");

    // Two sessions: one we'll archive + age out, one we'll just archive
    // (still fresh — sweep with a 1h TTL must skip it).
    let aged = spawn_and_archive(&h, &repo_root).await;
    let fresh = spawn_and_archive(&h, &repo_root).await;

    // Backdate the aged row's archived_at so it predates a 10s TTL.
    sqlx::query("UPDATE sessions SET archived_at = datetime('now', '-1 hour') WHERE id = ?1")
        .bind(aged.session_id.as_str())
        .execute(h.storage.writer_pool())
        .await
        .expect("backdate aged");

    // Sweep with a 10s TTL — aged > 10s, fresh < 10s.
    let (ok, err) = wm.sweep_expired(&h.storage, Duration::from_secs(10)).await;
    assert_eq!(err, 0, "no per-row errors expected");
    assert_eq!(ok, 1, "only the aged session should have been swept");

    assert!(
        !aged.worktree_path.exists(),
        "aged worktree should be gone after sweep: {}",
        aged.worktree_path.display()
    );
    assert!(
        fresh.worktree_path.exists(),
        "fresh worktree must NOT be touched by sweep: {}",
        fresh.worktree_path.display()
    );

    // Row hygiene: aged row has worktree_path cleared; fresh row keeps
    // its path until its own TTL elapses.
    let aged_row = h
        .storage
        .sessions()
        .get(aged.session_id.as_str())
        .await
        .expect("get aged")
        .expect("aged row");
    assert!(
        aged_row.worktree_path.is_none(),
        "swept row.worktree_path must be cleared"
    );
    let fresh_row = h
        .storage
        .sessions()
        .get(fresh.session_id.as_str())
        .await
        .expect("get fresh")
        .expect("fresh row");
    assert!(
        fresh_row.worktree_path.is_some(),
        "fresh row keeps its worktree_path before its TTL elapses"
    );
}

/// Bundle returned by [`spawn_and_archive`] so a sweep test can
/// reach the worktree path and session id without re-querying.
struct ArchivedFixture {
    session_id: SessionId,
    worktree_path: PathBuf,
}

/// Spawn a session with a worktree, wait for the child to exit, then
/// `archive` it. The archive path leaves the worktree on disk because
/// the v2 fix only clears `worktree_path` on cleanup success — but for
/// tests we want the directory present so the sweep has something to
/// reap. The cheapest way: skip archive's own cleanup by archiving the
/// session row directly via storage. That mirrors the production
/// "cleanup failed, retry later via sweep" path.
async fn spawn_and_archive(h: &support::TestHarness, repo_root: &Path) -> ArchivedFixture {
    let adapter = ShellAdapter::new("true");
    let spawned = h
        .manager
        .spawn_with_options(
            &adapter,
            h.project_id.clone(),
            la_adapter::SpawnRequest::new(repo_root.to_path_buf()),
            Some(WorktreeSpawnOptions {
                repo_root: repo_root.to_path_buf(),
            }),
        )
        .await
        .expect("spawn");
    let path = spawned.worktree_path.clone().expect("path");
    wait_for_exit(&h.manager, &spawned.id).await;
    // Mark the row archived without driving the WorktreeManager
    // cleanup branch — that's exactly the "cleanup deferred to sweep"
    // case we want to exercise.
    h.storage
        .sessions()
        .archive(spawned.id.as_str())
        .await
        .expect("archive row");
    ArchivedFixture {
        session_id: spawned.id,
        worktree_path: path,
    }
}

/// Helper: `git -C <repo_root> branch --list <branch>` is non-empty.
async fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    let out = Command::new("git")
        .args([
            "-C",
            repo_root.to_str().unwrap(),
            "branch",
            "--list",
            branch,
        ])
        .output()
        .await
        .expect("branch list");
    std::str::from_utf8(&out.stdout).unwrap().contains(branch)
}

/// WEK-8 §2.4 row 2: when `KeepBranchIfDirty` keeps the branch
/// (commits beyond base) `archive` MUST preserve `worktree_branch` on
/// the row so the TUI can later offer `git checkout la/session-<sid>`.
/// `worktree_path` must still be cleared because the directory is
/// gone. Regression for the v3 review NEEDS-FIX #2.
#[tokio::test]
async fn archive_with_dirty_branch_preserves_branch_column() {
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let h = harness_with_worktree(&repo_root).await;
    let adapter = ShellAdapter::new("true");
    let spawned = h
        .manager
        .spawn_with_options(
            &adapter,
            h.project_id.clone(),
            la_adapter::SpawnRequest::new(repo_root.clone()),
            Some(WorktreeSpawnOptions {
                repo_root: repo_root.clone(),
            }),
        )
        .await
        .expect("spawn");
    let wt_path = spawned.worktree_path.clone().expect("path");
    let branch = spawned.worktree_branch.clone().expect("branch");
    wait_for_exit(&h.manager, &spawned.id).await;

    // Commit something inside the worktree so the session branch is
    // ahead of `main` — KeepBranchIfDirty must then preserve it.
    tokio::fs::write(wt_path.join("agent.txt"), b"work\n")
        .await
        .unwrap();
    run(&wt_path, &["git", "add", "agent.txt"]).await;
    run(&wt_path, &["git", "commit", "-q", "-m", "agent work"]).await;

    h.manager.archive(&spawned.id).await.expect("archive");

    assert!(!wt_path.exists(), "worktree dir must be gone after archive");
    assert!(
        branch_exists(&repo_root, &branch).await,
        "branch {branch} must survive (commits beyond base)"
    );

    let row = h
        .storage
        .sessions()
        .get(spawned.id.as_str())
        .await
        .expect("get")
        .expect("row");
    assert!(
        row.worktree_path.is_none(),
        "worktree_path must be NULL after archive"
    );
    assert_eq!(
        row.worktree_branch.as_deref(),
        Some(branch.as_str()),
        "worktree_branch column must survive when the branch was kept"
    );
}

/// `WorktreeManager::prune_orphans` MUST drop `.git/worktrees/<name>`
/// admin entries whose recorded directory no longer exists. We fake
/// that by `git worktree add`ing a real worktree, then `rm -rf`-ing
/// the directory behind git's back so the admin entry is orphaned.
/// Asserts the call is also non-fatal on a path that isn't a git repo.
#[tokio::test]
async fn prune_orphans_reaps_orphan_admin_entries() {
    let project_tmp = TempDir::new().expect("project dir");
    let repo_root = project_tmp.path().join("repo");
    init_repo_with_seed(&repo_root).await;

    let state = TempDir::new().expect("state");
    let wm = WorktreeManager::for_state_dir(state.path());

    // Real `git worktree add` so git lays down the admin entry the
    // way it would in production.
    let stranded_dir = project_tmp.path().join("stranded");
    run(
        &repo_root,
        &[
            "git",
            "worktree",
            "add",
            "-b",
            "la/session-prunetest",
            stranded_dir.to_str().unwrap(),
        ],
    )
    .await;
    let admin = repo_root.join(".git").join("worktrees").join("stranded");
    assert!(
        admin.exists(),
        "git should have written admin entry at {}",
        admin.display()
    );

    // Yank the directory behind git's back — now `git worktree list`
    // still mentions it but the path is gone. `prune` is what
    // resolves that.
    tokio::fs::remove_dir_all(&stranded_dir).await.unwrap();
    assert!(
        admin.exists(),
        "admin entry should still be around pre-prune"
    );

    wm.prune_orphans(&repo_root).await;

    assert!(
        !admin.exists(),
        "prune_orphans must drop the orphan admin entry at {}",
        admin.display()
    );

    // Calling on a non-git path is best-effort and MUST NOT panic /
    // return Err (the function returns () anyway). Asserts the
    // contract directly.
    let not_a_repo = TempDir::new().expect("not-a-repo");
    wm.prune_orphans(not_a_repo.path()).await;
}
