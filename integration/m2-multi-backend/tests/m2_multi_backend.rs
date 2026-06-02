//! WEK-30 / M2.7 — multi-backend + isolated worktree + diff review path.
//!
//! Scope is intentionally Linux-only for v1 per WEK-5: the production code
//! remains portable, but this acceptance test uses `/bin/sh` as a deterministic
//! long-running mock backend.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet};
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
use la_proto::error_codes;
use la_proto::jsonrpc::{Message, Notification, Request, RequestId, ResponseOutcome, RpcError};
use la_proto::methods::{
    EventTopic, EventsSubscribeParams, SessionsCreateParams, SessionsCreateResult,
    WorktreeCommitParams, WorktreeCommitResult, WorktreeDiffParams, WorktreeDiffResult,
    WorktreeMutationParams, WorktreeMutationResult, WorktreeStatusParams, WorktreeStatusResult,
};
use la_proto::notifications::{
    BackendHealth as WireBackendHealth, BackendHealthStatus, DaemonHealth, DaemonHealthParams,
    NotificationMethod,
};
use la_tui::{
    runner::draw, App, AppMsg, BackendBadge, DiffAction, DiffKey, DiffPayload, DiffView,
    MockSessionSource,
};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tempfile::TempDir;
use tokio::time::timeout;

const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const AVAILABLE_BACKENDS: [&str; 3] = ["mock-alpha", "mock-beta", "mock-gamma"];
const UNAVAILABLE_BACKEND: &str = "mock-offline";

struct MockBackend {
    id: &'static str,
    display_name: &'static str,
    probe: ProbeResult,
}

#[async_trait]
impl AgentAdapter for MockBackend {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: self.id,
            display_name: self.display_name,
            default_program: "/bin/sh",
            docs_url: "https://example.test/mock-backend",
        }
    }

    async fn probe(&self) -> ProbeResult {
        self.probe.clone()
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        Ok(SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "sleep 60".into()],
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
    handle: DaemonHandle,
    join: tokio::task::JoinHandle<()>,
    tempdir: TempDir,
}

impl TestDaemon {
    async fn shutdown(self) {
        self.handle.shutdown();
        let _ = timeout(Duration::from_secs(5), self.join).await;
    }
}

async fn bootstrap() -> TestDaemon {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket = runtime_dir.join("lad-1.sock");

    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    for id in AVAILABLE_BACKENDS {
        adapters.insert(
            id.to_string(),
            Arc::new(MockBackend {
                id,
                display_name: id,
                probe: ProbeResult::Available {
                    version: "test-1.0.0".into(),
                },
            }),
        );
    }
    adapters.insert(
        UNAVAILABLE_BACKEND.to_string(),
        Arc::new(MockBackend {
            id: UNAVAILABLE_BACKEND,
            display_name: "Mock Offline",
            probe: ProbeResult::NotInstalled {
                hint: "mock-offline binary missing".into(),
            },
        }),
    );

    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        probe_interval: Duration::from_millis(200),
        shutdown_deadline: Duration::from_secs(2),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind daemon");
    let (handle, join) = daemon.spawn();
    wait_for_socket(&socket).await;
    TestDaemon {
        socket,
        handle,
        join,
        tempdir,
    }
}

async fn wait_for_socket(socket: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if connect(&Endpoint::uds(socket)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "daemon socket did not become connectable: {}",
        socket.display()
    );
}

async fn client(socket: &Path) -> Connection<tokio::net::UnixStream> {
    let stream = connect(&Endpoint::uds(socket)).await.expect("connect");
    let mut conn = Connection::new(stream);
    let info = client_handshake(
        &mut conn,
        "wek30-test",
        "0.0.0",
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .expect("handshake");
    assert_eq!(info.protocol_version, la_proto::PROTOCOL_VERSION);
    assert!(info.capabilities.worktree);
    assert!(info.capabilities.diff);
    for id in AVAILABLE_BACKENDS {
        assert!(info.capabilities.adapters.contains(&id.to_string()));
    }
    assert!(info
        .capabilities
        .adapters
        .contains(&UNAVAILABLE_BACKEND.to_string()));
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
                ResponseOutcome::Error(e) => panic!("rpc error for {method}: {e:?}"),
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

async fn subscribe_and_wait_for_health(
    conn: &mut Connection<tokio::net::UnixStream>,
) -> Vec<WireBackendHealth> {
    let req = Request::new(
        900,
        "events.subscribe".to_string(),
        &EventsSubscribeParams {
            topics: vec![EventTopic::DaemonHealth],
        },
    )
    .expect("encode subscribe");
    conn.send(&Message::Request(req))
        .await
        .expect("send subscribe");

    let mut saw_response = false;
    let deadline = tokio::time::Instant::now() + RPC_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = timeout(remaining, conn.recv())
            .await
            .expect("health timeout")
            .expect("health recv")
            .expect("health eof");
        match msg {
            Message::Response(resp) if resp.id == RequestId::Num(900) => match resp.outcome {
                ResponseOutcome::Result(_) => saw_response = true,
                ResponseOutcome::Error(e) => panic!("subscribe failed: {e:?}"),
            },
            Message::Notification(Notification { method, params, .. })
                if method == DaemonHealth::NAME =>
            {
                let Some(params) = params else { continue };
                let health: DaemonHealthParams =
                    serde_json::from_value(params).expect("decode daemon.health");
                if health.backends.len() == 4
                    && health.backends.iter().any(|b| {
                        b.id == UNAVAILABLE_BACKEND && b.status == BackendHealthStatus::NotInstalled
                    })
                    && saw_response
                {
                    return health.backends;
                }
            }
            _ => {}
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
    run_git(&repo, &["config", "user.email", "t@example.com"]).await;
    run_git(&repo, &["config", "user.name", "tester"]).await;
    run_git(&repo, &["config", "commit.gpgsign", "false"]).await;
    tokio::fs::write(repo.join("shared.txt"), "base\n")
        .await
        .unwrap();
    run_git(&repo, &["add", "."]).await;
    run_git(&repo, &["commit", "-q", "-m", "seed"]).await;
    (td, repo)
}

fn render_backends_to_text(backends: Vec<WireBackendHealth>) -> String {
    let badges: Vec<BackendBadge> = backends.iter().map(BackendBadge::from_wire).collect();
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = App::new(MockSessionSource::fixture());
    app.handle(AppMsg::BackendsUpdate(badges));
    terminal
        .draw(|f| {
            let _ = draw(f, &app);
        })
        .expect("draw");

    let buf = terminal.backend().buffer();
    let area = buf.area();
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

async fn create_session(
    socket: PathBuf,
    repo: PathBuf,
    backend: &'static str,
    request_id: i64,
) -> SessionsCreateResult {
    let mut conn = client(&socket).await;
    call(
        &mut conn,
        request_id,
        "sessions.create",
        SessionsCreateParams {
            project_dir: repo.to_string_lossy().into_owned(),
            backend: backend.into(),
            args: vec![],
            prompt: None,
            worktree: true,
        },
    )
    .await
}

async fn review_stage_and_commit(
    conn: &mut Connection<tokio::net::UnixStream>,
    session: &SessionsCreateResult,
    backend: &str,
    request_base: i64,
) -> String {
    let status: WorktreeStatusResult = call(
        conn,
        request_base,
        "worktree.status",
        WorktreeStatusParams {
            session_id: session.session_id.clone(),
        },
    )
    .await;
    assert_eq!(status.files.len(), 1, "status for {backend}: {status:?}");
    assert_eq!(status.files[0].path, "shared.txt");

    let diff: WorktreeDiffResult = call(
        conn,
        request_base + 1,
        "worktree.diff",
        WorktreeDiffParams {
            session_id: session.session_id.clone(),
            path: "shared.txt".into(),
            staged: false,
            context_lines: None,
        },
    )
    .await;
    assert_eq!(diff.hunks.len(), 1, "diff for {backend}: {diff:?}");
    assert!(
        diff.hunks[0]
            .lines
            .iter()
            .any(|line| line.content.contains(backend)),
        "diff hunk should contain backend-specific agent edit"
    );

    let mut view = DiffView::new();
    view.apply_status(status.files);
    view.apply_diff(DiffPayload {
        file: diff.file,
        hunks: diff.hunks,
        truncated: diff.truncated,
    });
    assert_eq!(
        view.files.len(),
        1,
        "TUI diff panel should list the changed file"
    );
    assert_eq!(view.files[0].entry.path, "shared.txt");
    assert_eq!(
        view.files[0].hunks.len(),
        1,
        "TUI diff panel should show the hunk"
    );
    view.cycle_focus();
    let DiffAction::Stage { hunk_id } = view.handle_key(DiffKey::Stage) else {
        panic!("TUI stage key should target the loaded hunk");
    };

    let stage: WorktreeMutationResult = call(
        conn,
        request_base + 2,
        "worktree.stage",
        WorktreeMutationParams {
            session_id: session.session_id.clone(),
            hunk_ids: vec![hunk_id],
            confirmed: false,
        },
    )
    .await;
    assert_eq!(stage.applied.len(), 1, "stage for {backend}: {stage:?}");

    let commit: WorktreeCommitResult = call(
        conn,
        request_base + 3,
        "worktree.commit",
        WorktreeCommitParams {
            session_id: session.session_id.clone(),
            message: format!("test: commit {backend} edit"),
            allow_empty: false,
        },
    )
    .await;
    assert_eq!(commit.commit_sha.len(), 40);
    assert_eq!(commit.files_changed, 1);
    commit.commit_sha
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_mock_backends_run_in_isolated_worktrees_and_diff_flow_commits() {
    let (_project_tmp, repo) = make_project_repo().await;
    let daemon = bootstrap().await;

    let mut control = client(&daemon.socket).await;
    let health = subscribe_and_wait_for_health(&mut control).await;
    let rendered = render_backends_to_text(health);
    assert!(
        rendered.contains("mock-alpha"),
        "available backend missing:\n{rendered}"
    );
    assert!(
        rendered.contains("Mock Offline"),
        "offline backend missing:\n{rendered}"
    );
    assert!(
        rendered.contains("not installed"),
        "unavailable backend must render as grey-state:\n{rendered}"
    );
    assert!(
        rendered.contains("mock-offline binary missing"),
        "grey-state reason should be visible:\n{rendered}"
    );

    let unavailable_error = call_expect_err(
        &mut control,
        901,
        "sessions.create",
        SessionsCreateParams {
            project_dir: repo.to_string_lossy().into_owned(),
            backend: UNAVAILABLE_BACKEND.into(),
            args: vec![],
            prompt: None,
            worktree: true,
        },
    )
    .await;
    assert_eq!(unavailable_error.code, error_codes::ADAPTER_NOT_INSTALLED);

    let (alpha, beta, gamma) = tokio::join!(
        create_session(daemon.socket.clone(), repo.clone(), "mock-alpha", 1),
        create_session(daemon.socket.clone(), repo.clone(), "mock-beta", 2),
        create_session(daemon.socket.clone(), repo.clone(), "mock-gamma", 3),
    );
    let sessions = vec![alpha, beta, gamma];
    let cwd_set: HashSet<_> = sessions.iter().map(|s| s.cwd.clone()).collect();
    assert_eq!(
        cwd_set.len(),
        3,
        "each backend must get an isolated worktree"
    );
    for s in &sessions {
        assert!(s
            .cwd
            .starts_with(daemon.tempdir.path().to_string_lossy().as_ref()));
        assert_ne!(Path::new(&s.cwd), repo.as_path());
    }

    for session in &sessions {
        tokio::fs::write(
            Path::new(&session.cwd).join("shared.txt"),
            format!("agent edit from {}\n", session.backend),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        tokio::fs::read_to_string(repo.join("shared.txt"))
            .await
            .unwrap(),
        "base\n",
        "agent edits must not leak into the original checkout"
    );

    let mut conn = client(&daemon.socket).await;
    let mut commits = Vec::new();
    for (idx, session) in sessions.iter().enumerate() {
        commits.push(
            review_stage_and_commit(&mut conn, session, &session.backend, 100 + idx as i64 * 10)
                .await,
        );
    }
    assert_eq!(
        commits.iter().collect::<HashSet<_>>().len(),
        3,
        "each worktree branch should receive its own commit"
    );

    daemon.shutdown().await;
}
