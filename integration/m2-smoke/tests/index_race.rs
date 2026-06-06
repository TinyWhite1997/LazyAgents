mod common;

use std::path::PathBuf;

use common::{
    bootstrap_daemon, call, client, make_bare_project_repo, standard_backends, write_agent_change,
};
use la_proto::jsonrpc::{Message, Request, RequestId, ResponseOutcome};
use la_proto::methods::{
    SessionsCreateParams, SessionsCreateResult, SessionsListParams, SessionsListResult,
    WorktreeDiffParams, WorktreeDiffResult, WorktreeMutationParams, WorktreeMutationResult,
    WorktreeStatusParams, WorktreeStatusResult,
};
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_stage_same_hunk_does_not_corrupt_index_or_session_row() {
    let daemon = bootstrap_daemon(standard_backends()).await;
    let mut observed_contention = false;

    for round in 0..32 {
        let outcomes = run_stage_race_round(&daemon, round).await;
        assert!(
            outcomes.iter().all(StageOutcome::is_safe),
            "unexpected race outcomes in round {round}: {outcomes:?}"
        );
        assert!(
            outcomes.iter().any(|o| matches!(o, StageOutcome::Applied)),
            "at least one concurrent stage should apply the hunk in round {round}: {outcomes:?}"
        );
        observed_contention |= outcomes
            .iter()
            .any(|o| matches!(o, StageOutcome::Stale | StageOutcome::StorageBusy));
        if observed_contention {
            break;
        }
    }

    assert!(
        observed_contention,
        "32 concurrent stage attempts never observed a stale/storage-busy loser"
    );
}

async fn run_stage_race_round(daemon: &common::TestDaemon, round: usize) -> Vec<StageOutcome> {
    let (_project, repo) = make_bare_project_repo().await;
    let mut conn = client(&daemon.socket).await;
    let created: SessionsCreateResult = call(
        &mut conn,
        1 + round as i64 * 10,
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
    let wt = PathBuf::from(&created.cwd);
    let path = write_agent_change(&wt, "claude", "race").await;
    let diff: WorktreeDiffResult = call(
        &mut conn,
        2 + round as i64 * 10,
        "worktree.diff",
        WorktreeDiffParams {
            session_id: created.session_id.clone(),
            path,
            staged: false,
            context_lines: None,
        },
    )
    .await;
    let hunk_id = diff.hunks[0].hunk_id.clone();

    let socket_a = daemon.socket.clone();
    let socket_b = daemon.socket.clone();
    let session_a = created.session_id.clone();
    let session_b = created.session_id.clone();
    let hunk_a = hunk_id.clone();
    let hunk_b = hunk_id;
    let (left, right) = tokio::join!(
        tokio::spawn(async move { stage_once(&socket_a, 3, session_a, hunk_a).await }),
        tokio::spawn(async move { stage_once(&socket_b, 4, session_b, hunk_b).await })
    );
    let outcomes = vec![left.expect("left join"), right.expect("right join")];

    let status: WorktreeStatusResult = call(
        &mut conn,
        5 + round as i64 * 10,
        "worktree.status",
        WorktreeStatusParams {
            session_id: created.session_id.clone(),
        },
    )
    .await;
    assert_eq!(
        status.files.iter().map(|f| f.staged_hunks).sum::<u32>(),
        1,
        "index should contain exactly one staged hunk after racing the same hunk"
    );

    let listed: SessionsListResult = call(
        &mut conn,
        6 + round as i64 * 10,
        "sessions.list",
        SessionsListParams {
            project: None,
            backend: Some("claude".into()),
            include_archived: true,
        },
    )
    .await;
    let row = listed
        .sessions
        .iter()
        .find(|s| s.session_id == created.session_id)
        .expect("session row still present");
    assert_eq!(row.worktree_path.as_deref(), Some(created.cwd.as_str()));
    outcomes
}

#[derive(Debug)]
enum StageOutcome {
    Applied,
    Stale,
    StorageBusy,
}

impl StageOutcome {
    fn is_safe(&self) -> bool {
        matches!(
            self,
            StageOutcome::Applied | StageOutcome::Stale | StageOutcome::StorageBusy
        )
    }
}

async fn stage_once(
    socket: &std::path::Path,
    id: i64,
    session_id: String,
    hunk_id: String,
) -> StageOutcome {
    let mut conn = client(socket).await;
    let req = Request::new(
        id,
        "worktree.stage".to_string(),
        &WorktreeMutationParams {
            session_id,
            hunk_ids: vec![hunk_id],
            confirmed: false,
        },
    )
    .expect("encode stage");
    conn.send(&Message::Request(req)).await.expect("send stage");
    loop {
        let msg = timeout(common::RPC_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("eof");
        let Message::Response(resp) = msg else {
            continue;
        };
        assert_eq!(resp.id, RequestId::Num(id));
        return match resp.outcome {
            ResponseOutcome::Result(v) => {
                let result: WorktreeMutationResult = serde_json::from_value(v).expect("decode");
                if result.applied.is_empty() {
                    StageOutcome::Stale
                } else {
                    StageOutcome::Applied
                }
            }
            ResponseOutcome::Error(e) if e.code == la_proto::error_codes::WORKTREE_HUNK_STALE => {
                StageOutcome::Stale
            }
            ResponseOutcome::Error(e) if e.code == la_proto::error_codes::STORAGE_BUSY => {
                StageOutcome::StorageBusy
            }
            ResponseOutcome::Error(e) => panic!("unexpected stage race rpc error: {e:?}"),
        };
    }
}
