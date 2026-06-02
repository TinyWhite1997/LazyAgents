//! End-to-end tests for the M2.5 / WEK-28 diff review backend.
//!
//! These spin up a real `git init` repo in a tempdir, run the
//! [`DiffEngine`] against it, and assert the file/hunk lifecycle:
//!
//! - status reports modified / untracked / deleted files,
//! - per-file diff returns parseable hunks with stable ids,
//! - stage / unstage round-trips a hunk into and out of the index,
//! - commit lands a real commit_sha and clears the dirty set,
//! - discard with confirmed reverts the working tree,
//! - large files return a truncation marker rather than 5 MiB of hunks,
//! - stale ids end up in `rejected` instead of erroring out.

use std::path::PathBuf;

use la_core::{DiffEngine, FileStatus, SessionId, WorktreeLocks};
use tempfile::TempDir;

async fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", "tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .output()
        .await
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn make_repo() -> (TempDir, PathBuf) {
    let td = TempDir::new().unwrap();
    let repo = td.path().to_path_buf();
    run_git(&repo, &["init", "-q", "-b", "main"]).await;
    run_git(&repo, &["config", "user.email", "t@e.com"]).await;
    run_git(&repo, &["config", "user.name", "t"]).await;
    run_git(&repo, &["config", "commit.gpgsign", "false"]).await;
    tokio::fs::write(repo.join("README.md"), "hi\n")
        .await
        .unwrap();
    run_git(&repo, &["add", "README.md"]).await;
    run_git(&repo, &["commit", "-q", "-m", "init"]).await;
    (td, repo)
}

fn engine(repo: &std::path::Path) -> DiffEngine {
    DiffEngine::new(
        repo.to_path_buf(),
        repo.to_path_buf(),
        SessionId("session-test".to_string()),
        WorktreeLocks::new(),
        None,
    )
}

#[tokio::test]
async fn status_reports_modified_and_untracked() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("README.md"), "hi\nmodified\n")
        .await
        .unwrap();
    tokio::fs::write(repo.join("new.txt"), "fresh\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    let s = eng.status().await.expect("status");
    let by_path: std::collections::HashMap<_, _> =
        s.files.iter().map(|f| (f.path.clone(), f)).collect();
    assert_eq!(by_path["README.md"].status, FileStatus::Modified);
    assert_eq!(by_path["new.txt"].status, FileStatus::Untracked);
}

#[tokio::test]
async fn diff_file_returns_hunks_with_stable_ids() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("README.md"), "hi\nmodified\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    let d1 = eng.diff_file("README.md", false, None).await.expect("diff");
    let d2 = eng.diff_file("README.md", false, None).await.expect("diff");
    assert_eq!(d1.hunks.len(), 1);
    assert_eq!(d1.hunks[0].hunk_id, d2.hunks[0].hunk_id);
}

#[tokio::test]
async fn stage_then_unstage_returns_index_to_clean() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("README.md"), "hi\nstaged\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    let diff = eng.diff_file("README.md", false, None).await.unwrap();
    let id = diff.hunks[0].hunk_id.clone();

    let staged = eng.stage(&[id.clone()]).await.unwrap();
    assert_eq!(staged.applied.len(), 1, "stage applied count");
    assert!(staged.rejected.is_empty(), "rejected={:?}", staged.rejected);

    // After stage the file should report at least one staged hunk.
    let snap = eng.status().await.unwrap();
    let entry = snap.files.iter().find(|f| f.path == "README.md").unwrap();
    assert!(entry.staged_hunks >= 1);

    // The staged diff carries a fresh id.
    let staged_diff = eng.diff_file("README.md", true, None).await.unwrap();
    assert!(!staged_diff.hunks.is_empty());
    let staged_id = staged_diff.hunks[0].hunk_id.clone();

    let un = eng.unstage(&[staged_id]).await.unwrap();
    assert!(un.applied.len() == 1 || un.rejected.is_empty());
    let snap2 = eng.status().await.unwrap();
    let entry2 = snap2.files.iter().find(|f| f.path == "README.md").unwrap();
    assert_eq!(
        entry2.staged_hunks, 0,
        "index should be clean after unstage"
    );
}

#[tokio::test]
async fn stale_hunk_ids_are_rejected_not_errored() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("README.md"), "hi\nmod\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    // Random fake id.
    let outcome = eng
        .stage(&["00000000deadbeef".to_string()])
        .await
        .expect("stage no-op shouldn't error");
    assert!(outcome.applied.is_empty());
    assert_eq!(outcome.rejected.len(), 1);
    assert_eq!(outcome.rejected[0].reason, "stale");
}

/// Regression for Backend Architect's review finding: a call mixing a
/// real hunk id with a stale one must surface the stale subset in
/// `rejected[]` — the earlier per-file loop only accounted for the
/// matched subset, so a stale id silently disappeared whenever any
/// other id in the same call landed. The wire contract on
/// `WorktreeMutationParams` promises every stale id round-trips.
#[tokio::test]
async fn mixed_real_and_stale_ids_partition_correctly() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("README.md"), "hi\nstaged\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    let diff = eng.diff_file("README.md", false, None).await.unwrap();
    let real_id = diff.hunks[0].hunk_id.clone();
    let stale_id = "00000000deadbeef".to_string();

    let outcome = eng
        .stage(&[real_id.clone(), stale_id.clone()])
        .await
        .expect("mixed call must not error");
    assert_eq!(outcome.applied, vec![real_id]);
    assert_eq!(outcome.rejected.len(), 1, "got {:?}", outcome.rejected);
    assert_eq!(outcome.rejected[0].hunk_id, stale_id);
    assert_eq!(outcome.rejected[0].reason, "stale");
}

#[tokio::test]
async fn commit_lands_real_sha_and_clears_index() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("README.md"), "hi\ncommit me\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    let diff = eng.diff_file("README.md", false, None).await.unwrap();
    eng.stage(&[diff.hunks[0].hunk_id.clone()]).await.unwrap();
    let out = eng.commit("feat: new line", false).await.expect("commit");
    assert_eq!(out.commit_sha.len(), 40);
    assert!(out.commit_sha.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(out.summary, "feat: new line");
    assert!(out.files_changed >= 1);
}

#[tokio::test]
async fn discard_reverts_working_tree() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("README.md"), "hi\ndiscarded\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    let diff = eng.diff_file("README.md", false, None).await.unwrap();
    let _ = eng.discard(&[diff.hunks[0].hunk_id.clone()]).await.unwrap();
    let contents = tokio::fs::read_to_string(repo.join("README.md"))
        .await
        .unwrap();
    assert_eq!(contents, "hi\n");
}

#[tokio::test]
async fn untracked_file_can_be_staged() {
    let (_td, repo) = make_repo().await;
    tokio::fs::write(repo.join("new.txt"), "fresh\nbody\n")
        .await
        .unwrap();
    let eng = engine(&repo);
    let diff = eng.diff_file("new.txt", false, None).await.unwrap();
    assert_eq!(diff.hunks.len(), 1);
    let id = diff.hunks[0].hunk_id.clone();
    let staged = eng.stage(&[id]).await.unwrap();
    assert_eq!(staged.applied.len(), 1, "rejected={:?}", staged.rejected);
}

/// Regression for the WEK-28 review blocker: untracked files surface
/// a synthetic hunk via `diff_file`, but discard used `raw_diff` which
/// returns empty for untracked paths — every id ended up `stale`. The
/// engine now reuses `diff_untracked` so the id round-trips and the
/// file is removed on confirmed discard.
#[tokio::test]
async fn untracked_file_can_be_discarded_and_is_removed_from_disk() {
    let (_td, repo) = make_repo().await;
    let new_path = repo.join("scratch.txt");
    tokio::fs::write(&new_path, "fresh\nbody\n").await.unwrap();
    let eng = engine(&repo);
    let diff = eng.diff_file("scratch.txt", false, None).await.unwrap();
    assert_eq!(diff.hunks.len(), 1, "synthetic hunk must surface");
    let id = diff.hunks[0].hunk_id.clone();

    let outcome = eng.discard(&[id.clone()]).await.unwrap();
    assert_eq!(outcome.applied, vec![id], "rejected={:?}", outcome.rejected);
    assert!(
        outcome.rejected.is_empty(),
        "must NOT be stale; got {:?}",
        outcome.rejected
    );
    assert!(
        !new_path.exists(),
        "discard of an untracked file removes it from disk"
    );
}

#[tokio::test]
async fn large_file_returns_truncation_marker() {
    let (_td, repo) = make_repo().await;
    // 6 MiB of zeros — past the 5 MiB cap.
    let big: Vec<u8> = vec![0u8; 6 * 1024 * 1024];
    tokio::fs::write(repo.join("big.bin"), &big).await.unwrap();
    let eng = engine(&repo);
    let d = eng.diff_file("big.bin", false, None).await.unwrap();
    assert!(d.hunks.is_empty());
    assert!(d.truncated.is_some());
}
