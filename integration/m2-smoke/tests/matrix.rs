#![cfg(unix)]

mod common;

use std::path::PathBuf;

use common::{
    bootstrap_daemon, call, client, make_bare_project_repo, standard_backends, write_agent_change,
};
use la_proto::methods::{
    FileStatus, SessionsAttachParams, SessionsAttachResult, SessionsCreateParams,
    SessionsCreateResult, SessionsListParams, SessionsListResult, WorktreeCommitParams,
    WorktreeCommitResult, WorktreeDiffParams, WorktreeDiffResult, WorktreeMutationParams,
    WorktreeMutationResult, WorktreeStatusParams, WorktreeStatusResult,
};

#[derive(Debug, Clone, Copy)]
enum DiffOp {
    Stage,
    Unstage,
    Commit,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m2_adapter_worktree_diff_matrix() {
    let daemon = bootstrap_daemon(standard_backends()).await;
    let mut conn = client(&daemon.socket).await;
    let mut id = 1_i64;

    for backend in ["claude", "codex", "opencode"] {
        for worktree in [true, false] {
            for op in [DiffOp::Stage, DiffOp::Unstage, DiffOp::Commit] {
                let (_project, repo) = make_bare_project_repo().await;
                let created: SessionsCreateResult = call(
                    &mut conn,
                    next_id(&mut id),
                    "sessions.create",
                    SessionsCreateParams {
                        project_dir: repo.to_string_lossy().into_owned(),
                        backend: backend.into(),
                        args: vec![],
                        prompt: None,
                        worktree,
                    },
                )
                .await;
                assert_eq!(created.backend, backend);

                let listed: SessionsListResult = call(
                    &mut conn,
                    next_id(&mut id),
                    "sessions.list",
                    SessionsListParams {
                        project: None,
                        backend: Some(backend.into()),
                        include_archived: true,
                    },
                )
                .await;
                let row = listed
                    .sessions
                    .iter()
                    .find(|s| s.session_id == created.session_id)
                    .expect("created session appears in list");

                if !worktree {
                    assert!(row.worktree_path.is_none());
                    continue;
                }

                let worktree_path = PathBuf::from(&created.cwd);
                assert!(worktree_path.starts_with(daemon.tempdir.path()));
                assert_eq!(row.worktree_path.as_deref(), Some(created.cwd.as_str()));

                let changed = write_agent_change(&worktree_path, backend, op.label()).await;
                let status: WorktreeStatusResult = call(
                    &mut conn,
                    next_id(&mut id),
                    "worktree.status",
                    WorktreeStatusParams {
                        session_id: created.session_id.clone(),
                    },
                )
                .await;
                assert!(
                    status.files.iter().any(|f| f.path == changed
                        && matches!(
                            f.status,
                            FileStatus::Added | FileStatus::Modified | FileStatus::Untracked
                        )),
                    "status did not include {changed}: {status:?}"
                );

                let diff: WorktreeDiffResult = call(
                    &mut conn,
                    next_id(&mut id),
                    "worktree.diff",
                    WorktreeDiffParams {
                        session_id: created.session_id.clone(),
                        path: changed.clone(),
                        staged: false,
                        context_lines: None,
                    },
                )
                .await;
                assert!(
                    diff.hunks
                        .iter()
                        .any(|h| h.lines.iter().any(|l| l.content.contains(backend))),
                    "diff panel data missing backend text: {diff:?}"
                );

                let hunk_ids: Vec<String> = diff.hunks.iter().map(|h| h.hunk_id.clone()).collect();
                match op {
                    DiffOp::Stage => {
                        let staged: WorktreeMutationResult = call(
                            &mut conn,
                            next_id(&mut id),
                            "worktree.stage",
                            WorktreeMutationParams {
                                session_id: created.session_id.clone(),
                                hunk_ids,
                                confirmed: false,
                            },
                        )
                        .await;
                        assert_eq!(
                            staged.applied.len(),
                            1,
                            "stage rejected={:?}",
                            staged.rejected
                        );
                    }
                    DiffOp::Unstage => {
                        let staged: WorktreeMutationResult = call(
                            &mut conn,
                            next_id(&mut id),
                            "worktree.stage",
                            WorktreeMutationParams {
                                session_id: created.session_id.clone(),
                                hunk_ids: hunk_ids.clone(),
                                confirmed: false,
                            },
                        )
                        .await;
                        assert_eq!(
                            staged.applied.len(),
                            1,
                            "stage rejected={:?}",
                            staged.rejected
                        );
                        let unstaged: WorktreeMutationResult = call(
                            &mut conn,
                            next_id(&mut id),
                            "worktree.unstage",
                            WorktreeMutationParams {
                                session_id: created.session_id.clone(),
                                hunk_ids,
                                confirmed: false,
                            },
                        )
                        .await;
                        assert_eq!(
                            unstaged.applied.len(),
                            1,
                            "unstage rejected={:?}",
                            unstaged.rejected
                        );
                    }
                    DiffOp::Commit => {
                        let staged: WorktreeMutationResult = call(
                            &mut conn,
                            next_id(&mut id),
                            "worktree.stage",
                            WorktreeMutationParams {
                                session_id: created.session_id.clone(),
                                hunk_ids,
                                confirmed: false,
                            },
                        )
                        .await;
                        assert_eq!(
                            staged.applied.len(),
                            1,
                            "stage rejected={:?}",
                            staged.rejected
                        );
                        let commit: WorktreeCommitResult = call(
                            &mut conn,
                            next_id(&mut id),
                            "worktree.commit",
                            WorktreeCommitParams {
                                session_id: created.session_id.clone(),
                                message: format!("test: {backend} {changed}"),
                                allow_empty: false,
                            },
                        )
                        .await;
                        assert_eq!(commit.commit_sha.len(), 40);
                        assert_eq!(commit.files_changed, 1);
                    }
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_backends_run_concurrently_in_isolated_worktrees() {
    let daemon = bootstrap_daemon(standard_backends()).await;
    let mut handles = Vec::new();

    for backend in ["claude", "codex", "opencode"] {
        let socket = daemon.socket.clone();
        handles.push(tokio::spawn(async move {
            let (_project, repo) = make_bare_project_repo().await;
            let mut conn = client(&socket).await;
            let created: SessionsCreateResult = call(
                &mut conn,
                1,
                "sessions.create",
                SessionsCreateParams {
                    project_dir: repo.to_string_lossy().into_owned(),
                    backend: backend.into(),
                    args: vec![],
                    prompt: None,
                    worktree: true,
                },
            )
            .await;
            let wt = PathBuf::from(&created.cwd);
            let path = write_agent_change(&wt, backend, "parallel").await;
            let status: WorktreeStatusResult = call(
                &mut conn,
                2,
                "worktree.status",
                WorktreeStatusParams {
                    session_id: created.session_id,
                },
            )
            .await;
            assert!(status.files.iter().any(|f| f.path == path));
            (backend, wt)
        }));
    }

    let mut worktrees = Vec::new();
    for handle in handles {
        worktrees.push(handle.await.expect("join concurrent backend"));
    }
    for (i, (_, left)) in worktrees.iter().enumerate() {
        for (_, right) in worktrees.iter().skip(i + 1) {
            assert_ne!(left, right, "sessions must not share a worktree");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_detach_keeps_session_non_terminal() {
    let daemon = bootstrap_daemon(standard_backends()).await;
    let (_project, repo) = make_bare_project_repo().await;
    let mut conn = client(&daemon.socket).await;
    let created: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        SessionsCreateParams {
            project_dir: repo.to_string_lossy().into_owned(),
            backend: "claude".into(),
            args: vec![],
            prompt: None,
            worktree: true,
        },
    )
    .await;
    let _: SessionsAttachResult = call(
        &mut conn,
        2,
        "sessions.attach",
        SessionsAttachParams {
            session_id: created.session_id.clone(),
            resume_from_seq: None,
            replay_bytes: None,
            acquire_input: false,
        },
    )
    .await;
    drop(conn);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let mut conn = client(&daemon.socket).await;
    let listed: SessionsListResult = call(
        &mut conn,
        3,
        "sessions.list",
        SessionsListParams {
            project: None,
            backend: Some("claude".into()),
            include_archived: true,
        },
    )
    .await;
    assert!(
        listed
            .sessions
            .iter()
            .any(|s| s.session_id == created.session_id
                && matches!(
                    s.state,
                    la_proto::methods::SessionState::Starting
                        | la_proto::methods::SessionState::Running
                        | la_proto::methods::SessionState::Waiting
                )),
        "session should remain non-terminal after client detach; listed={listed:?}"
    );
}

impl DiffOp {
    fn label(self) -> &'static str {
        match self {
            DiffOp::Stage => "stage",
            DiffOp::Unstage => "unstage",
            DiffOp::Commit => "commit",
        }
    }
}

fn next_id(id: &mut i64) -> i64 {
    let out = *id;
    *id += 1;
    out
}
