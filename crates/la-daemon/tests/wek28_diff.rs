//! WEK-28 / M2.5 — `worktree.*` diff review surface end-to-end.
//!
//! Spins up a real daemon, creates a session with `worktree: true`
//! against a fresh git repo, mutates 10 files inside the worktree, and
//! drives the full diff/stage/commit/discard cycle through the
//! JSON-RPC client.
//!
//! Acceptance bar from the issue body:
//! > Story 'Agent edited 10 files' reviews smoothly
//! > Discard has 二次确认 (double confirmation)

// Talks to the daemon over UDS; the Windows pipe path is exercised
// by separate Windows-specific tests. Gating the whole file keeps
// the WEK-72 matrix CI green on windows-2022.
#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{
    AdapterDescriptor, AdapterError, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec,
};
use la_daemon::{Daemon, DaemonConfig, DaemonHandle, SocketDiscovery};
use la_ipc::transport::{connect, Endpoint};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request, RequestId, ResponseOutcome, RpcError};
use la_proto::methods::{
    SessionsCreateParams, SessionsCreateResult, WorktreeCommitParams, WorktreeCommitResult,
    WorktreeDiffParams, WorktreeDiffResult, WorktreeMutationParams, WorktreeMutationResult,
    WorktreeStatusParams, WorktreeStatusResult,
};
use tempfile::TempDir;
use tokio::time::timeout;

const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimal long-running adapter so `sessions.create` returns a live
/// session id we can use for `worktree.*` RPCs without depending on a
/// real backend binary.
struct IdleShell;

#[async_trait]
impl AgentAdapter for IdleShell {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "idle",
            display_name: "Idle Shell",
            default_program: "sh",
            docs_url: "https://example.test/idle",
        }
    }

    async fn probe(&self) -> ProbeResult {
        ProbeResult::Available {
            version: "0.0.0".into(),
        }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        // `sleep` keeps the PTY alive long enough for our RPC calls to
        // land; the test cleans up by dropping the daemon at end of run.
        let script_dir = std::env::temp_dir().join("lazyagents-wek28-test-scripts");
        std::fs::create_dir_all(&script_dir).map_err(AdapterError::SpawnFailed)?;
        let script_path = script_dir.join(format!("{}.sh", la_storage::new_id()));
        std::fs::write(&script_path, "#!/bin/sh\nsleep 60\n").map_err(AdapterError::SpawnFailed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
                .map_err(AdapterError::SpawnFailed)?;
        }
        Ok(SpawnSpec {
            program: script_path,
            args: Vec::new(),
            env: req.env.clone(),
            cwd: req.cwd.clone(),
            pty: req.pty,
            stdin_mode: req.stdin_mode,
        })
    }

    fn encode_user_input(&self, text: &str) -> Bytes {
        Bytes::copy_from_slice(text.as_bytes())
    }
}

struct TestDaemon {
    socket: PathBuf,
    _handle: DaemonHandle,
    _join: tokio::task::JoinHandle<()>,
    tempdir: TempDir,
}

async fn bootstrap() -> TestDaemon {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket = runtime_dir.join("lad-1.sock");
    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert("idle".to_string(), Arc::new(IdleShell));
    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        probe_interval: Duration::from_millis(500),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind daemon");
    let (handle, join) = daemon.spawn();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if connect(&Endpoint::uds(&socket)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    TestDaemon {
        socket,
        _handle: handle,
        _join: join,
        tempdir,
    }
}

async fn client(socket: &Path) -> Connection<tokio::net::UnixStream> {
    let stream = connect(&Endpoint::uds(socket)).await.expect("connect");
    let mut conn = Connection::new(stream);
    let info = client_handshake(
        &mut conn,
        "wek28-test",
        "0.0.0",
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .expect("handshake");
    assert_eq!(info.protocol_version, la_proto::PROTOCOL_VERSION);
    assert!(
        info.capabilities.diff,
        "WEK-28 capability must be on by default"
    );
    assert!(info.capabilities.worktree, "WEK-27 capability prereq");
    conn
}

async fn call<T, R>(
    conn: &mut Connection<tokio::net::UnixStream>,
    id: i64,
    method: &str,
    params: T,
) -> R
where
    T: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let req = Request::new(id, method.to_string(), &params).expect("encode");
    conn.send(&Message::Request(req)).await.expect("send");
    loop {
        let msg = timeout(RPC_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("eof");
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, RequestId::Num(id));
            return match resp.outcome {
                ResponseOutcome::Result(v) => serde_json::from_value(v).expect("decode"),
                ResponseOutcome::Error(e) => {
                    panic!("rpc error for {method}: {e:?}")
                }
            };
        }
    }
}

async fn call_expect_err(
    conn: &mut Connection<tokio::net::UnixStream>,
    id: i64,
    method: &str,
    params: impl serde::Serialize,
) -> RpcError {
    let req = Request::new(id, method.to_string(), &params).expect("encode");
    conn.send(&Message::Request(req)).await.expect("send");
    loop {
        let msg = timeout(RPC_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("eof");
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, RequestId::Num(id));
            return match resp.outcome {
                ResponseOutcome::Error(e) => e,
                ResponseOutcome::Result(v) => panic!("expected error, got {v:?}"),
            };
        }
    }
}

async fn run_git(cwd: &Path, args: &[&str]) {
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

async fn make_project_repo() -> (TempDir, PathBuf) {
    let td = tempfile::tempdir().expect("project tmp");
    let repo = td.path().to_path_buf();
    run_git(&repo, &["init", "-q", "-b", "main"]).await;
    run_git(&repo, &["config", "user.email", "t@e.com"]).await;
    run_git(&repo, &["config", "user.name", "t"]).await;
    run_git(&repo, &["config", "commit.gpgsign", "false"]).await;
    for i in 0..10 {
        let name = format!("file_{:02}.txt", i);
        tokio::fs::write(repo.join(&name), "line0\nline1\nline2\n")
            .await
            .unwrap();
    }
    run_git(&repo, &["add", "."]).await;
    run_git(&repo, &["commit", "-q", "-m", "seed"]).await;
    (td, repo)
}

#[tokio::test]
async fn wek28_full_diff_review_round_trip() {
    let (_proj, repo) = make_project_repo().await;

    let daemon = bootstrap().await;
    let mut conn = client(&daemon.socket).await;

    // Create a session in worktree mode.
    let created: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        SessionsCreateParams {
            project_dir: repo.to_string_lossy().into_owned(),
            backend: "idle".into(),
            args: vec![],
            prompt: None,
            worktree: true,
        },
    )
    .await;
    let session_id = created.session_id.clone();
    let wt: PathBuf = created.cwd.clone().into();
    assert!(
        wt.starts_with(daemon.tempdir.path()),
        "worktree cwd should be inside daemon state dir; got {}",
        wt.display()
    );

    // Simulate "agent edited 10 files" in the worktree.
    for i in 0..10 {
        let name = format!("file_{:02}.txt", i);
        tokio::fs::write(wt.join(&name), format!("line0\nMODIFIED-{i}\nline2\n"))
            .await
            .unwrap();
    }
    // Plus one brand-new file.
    tokio::fs::write(wt.join("new.txt"), "fresh\n")
        .await
        .unwrap();

    // worktree.status surfaces all 11 changes.
    let status: WorktreeStatusResult = call(
        &mut conn,
        2,
        "worktree.status",
        WorktreeStatusParams {
            session_id: session_id.clone(),
        },
    )
    .await;
    assert_eq!(status.files.len(), 11, "got {status:?}");
    assert!(status.branch.starts_with("la/session-"));

    // worktree.diff returns one hunk per modified file.
    let diff: WorktreeDiffResult = call(
        &mut conn,
        3,
        "worktree.diff",
        WorktreeDiffParams {
            session_id: session_id.clone(),
            path: "file_00.txt".into(),
            staged: false,
            context_lines: None,
        },
    )
    .await;
    assert_eq!(diff.hunks.len(), 1);
    let hunk_id = diff.hunks[0].hunk_id.clone();

    // worktree.discard without confirmed=true → error -33127.
    let err = call_expect_err(
        &mut conn,
        4,
        "worktree.discard",
        WorktreeMutationParams {
            session_id: session_id.clone(),
            hunk_ids: vec![hunk_id.clone()],
            confirmed: false,
        },
    )
    .await;
    assert_eq!(
        err.code,
        la_proto::error_codes::WORKTREE_DISCARD_UNCONFIRMED
    );

    // worktree.stage commits 5 hunks into the index.
    let mut staged_ids = Vec::new();
    for i in 0..5 {
        let path = format!("file_{:02}.txt", i);
        let d: WorktreeDiffResult = call(
            &mut conn,
            100 + i as i64,
            "worktree.diff",
            WorktreeDiffParams {
                session_id: session_id.clone(),
                path,
                staged: false,
                context_lines: None,
            },
        )
        .await;
        staged_ids.push(d.hunks[0].hunk_id.clone());
    }
    let stage: WorktreeMutationResult = call(
        &mut conn,
        200,
        "worktree.stage",
        WorktreeMutationParams {
            session_id: session_id.clone(),
            hunk_ids: staged_ids.clone(),
            confirmed: false,
        },
    )
    .await;
    assert_eq!(stage.applied.len(), 5, "rejected={:?}", stage.rejected);

    // worktree.commit lands a real commit_sha.
    let commit: WorktreeCommitResult = call(
        &mut conn,
        300,
        "worktree.commit",
        WorktreeCommitParams {
            session_id: session_id.clone(),
            message: "feat: edit five files".into(),
            allow_empty: false,
        },
    )
    .await;
    assert_eq!(commit.commit_sha.len(), 40);
    assert_eq!(commit.summary, "feat: edit five files");
    assert!(commit.files_changed >= 5);

    // worktree.discard with confirmed=true reverts the 6th file.
    let d: WorktreeDiffResult = call(
        &mut conn,
        400,
        "worktree.diff",
        WorktreeDiffParams {
            session_id: session_id.clone(),
            path: "file_05.txt".into(),
            staged: false,
            context_lines: None,
        },
    )
    .await;
    let disc_id = d.hunks[0].hunk_id.clone();
    let disc: WorktreeMutationResult = call(
        &mut conn,
        401,
        "worktree.discard",
        WorktreeMutationParams {
            session_id: session_id.clone(),
            hunk_ids: vec![disc_id],
            confirmed: true,
        },
    )
    .await;
    assert_eq!(disc.applied.len(), 1, "rejected={:?}", disc.rejected);
    let contents = tokio::fs::read_to_string(wt.join("file_05.txt"))
        .await
        .unwrap();
    assert_eq!(contents, "line0\nline1\nline2\n");
}

#[tokio::test]
async fn wek28_status_on_nonworktree_session_returns_unavailable() {
    let (_proj, repo) = make_project_repo().await;
    let daemon = bootstrap().await;
    let mut conn = client(&daemon.socket).await;

    let created: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        SessionsCreateParams {
            project_dir: repo.to_string_lossy().into_owned(),
            backend: "idle".into(),
            args: vec![],
            prompt: None,
            worktree: false,
        },
    )
    .await;
    let err = call_expect_err(
        &mut conn,
        2,
        "worktree.status",
        WorktreeStatusParams {
            session_id: created.session_id.clone(),
        },
    )
    .await;
    assert_eq!(err.code, la_proto::error_codes::WORKTREE_UNAVAILABLE);
}
