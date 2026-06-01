//! Acceptance suite for WEK-21 (M1.7) — `lad` assembly and startup.
//!
//! Covers the three M1.7 acceptance criteria from the issue body:
//!
//! 1. **End-to-end**: bind a daemon, do `initialize` + `sessions.create` +
//!    `sessions.attach`, observe `session.output` flowing back.
//! 2. **Same-host coexistence**: two daemons pinned to distinct protocol
//!    majors (and distinct runtime dirs) bind without conflict.
//! 3. **Graceful shutdown**: a daemon driving a live child cleans the
//!    child up within 10 s of `DaemonHandle::shutdown`.
//!
//! These tests bring the daemon up *in-process* (no external `lad`
//! binary needed) so they run cleanly under `cargo test` on the standard
//! Linux CI runner. The `daemonize` fork path is covered by a separate
//! test that exercises the actual `lad` binary; we mark it `#[ignore]`
//! when `LAD_BIN` is unset so PR CI doesn't depend on `cargo build`
//! having been run first.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{AdapterDescriptor, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec};
use la_daemon::{Daemon, DaemonConfig, SocketDiscovery};
use la_ipc::transport::{connect, Endpoint};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request, RequestId};
use la_proto::methods::{
    SessionsAttachParams, SessionsAttachResult, SessionsCreateParams, SessionsCreateResult,
    SessionsDetachParams, SessionsDetachResult,
};
use la_proto::notifications::SessionOutputParams;
use tempfile::TempDir;
use tokio::time::timeout;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Adapter that runs `/bin/sh -c <script>` — same trick `la-core` tests
/// use. Avoids needing a real claude CLI inside CI.
struct ShellAdapter {
    script: String,
}

#[async_trait]
impl AgentAdapter for ShellAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "shtest",
            display_name: "Shell Test Backend",
            default_program: "sh",
            docs_url: "https://example.test/shtest",
        }
    }

    async fn probe(&self) -> ProbeResult {
        ProbeResult::Available {
            version: "0.0.0".into(),
        }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, la_adapter::AdapterError> {
        Ok(SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), self.script.clone().into()],
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
    handle: la_daemon::DaemonHandle,
    join: tokio::task::JoinHandle<()>,
    _tempdir: TempDir,
}

async fn bootstrap_daemon(script: &str) -> TestDaemon {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();

    let socket = runtime_dir.join("lad-1.sock");
    let adapter: Arc<dyn AgentAdapter> = Arc::new(ShellAdapter {
        script: script.to_string(),
    });
    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert("shtest".to_string(), adapter);

    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind daemon");
    let (handle, join) = daemon.spawn();

    // Wait for the socket to be ready for connections (Listener::bind has
    // already returned, but allow the accept loop one tick to spin up).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if connect(&Endpoint::uds(&socket)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    TestDaemon {
        socket,
        handle,
        join,
        _tempdir: tempdir,
    }
}

async fn client(socket: &std::path::Path) -> Connection<tokio::net::UnixStream> {
    let stream = connect(&Endpoint::uds(socket))
        .await
        .expect("client connect");
    let mut conn = Connection::new(stream);
    let info = client_handshake(&mut conn, "la-test", "0.0.0", &[la_proto::PROTOCOL_VERSION])
        .await
        .expect("handshake");
    assert_eq!(info.protocol_version, la_proto::PROTOCOL_VERSION);
    conn
}

async fn send_request<T: serde::Serialize>(
    conn: &mut Connection<tokio::net::UnixStream>,
    id: i64,
    method: &str,
    params: T,
) {
    let req = Request::new(id, method.to_string(), &params).expect("encode");
    conn.send(&Message::Request(req)).await.expect("send");
}

async fn recv_response_for(
    conn: &mut Connection<tokio::net::UnixStream>,
    expected_id: i64,
) -> serde_json::Value {
    loop {
        let msg = timeout(PROBE_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("eof");
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, RequestId::Num(expected_id), "id mismatch");
            return match resp.outcome {
                la_proto::jsonrpc::ResponseOutcome::Result(v) => v,
                la_proto::jsonrpc::ResponseOutcome::Error(e) => panic!("rpc error: {e:?}"),
            };
        }
    }
}

#[tokio::test]
async fn end_to_end_create_and_attach() {
    let project = tempfile::tempdir().expect("project tmp");
    let daemon = bootstrap_daemon("printf 'hello from lad\\n'; sleep 0.2; printf 'bye\\n'").await;
    let mut conn = client(&daemon.socket).await;

    // sessions.create
    let create_params = SessionsCreateParams {
        project_dir: project.path().to_string_lossy().to_string(),
        backend: "shtest".to_string(),
        args: vec![],
        prompt: None,
        worktree: false,
    };
    send_request(&mut conn, 1, "sessions.create", &create_params).await;
    let result: SessionsCreateResult =
        serde_json::from_value(recv_response_for(&mut conn, 1).await).expect("decode");
    let session_id = result.session_id;

    // sessions.attach
    let attach_params = SessionsAttachParams {
        session_id: session_id.clone(),
        resume_from_seq: None,
        replay_bytes: None,
        acquire_input: false,
    };
    send_request(&mut conn, 2, "sessions.attach", &attach_params).await;
    let _attach: SessionsAttachResult =
        serde_json::from_value(recv_response_for(&mut conn, 2).await).expect("decode attach");

    // Drain session.output notifications until we've seen the expected text.
    let mut seen = Vec::<u8>::new();
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < drain_deadline && !contains(&seen, b"hello from lad") {
        let msg = match tokio::time::timeout(Duration::from_millis(500), conn.recv()).await {
            Ok(Ok(Some(m))) => m,
            _ => continue,
        };
        if let Message::Notification(n) = msg {
            if n.method == "session.output" {
                if let Some(params) = n.params.as_ref() {
                    let p: SessionOutputParams =
                        serde_json::from_value(params.clone()).expect("decode output");
                    let bytes = p.data_bytes().expect("base64");
                    seen.extend_from_slice(&bytes);
                }
            }
        }
    }
    assert!(
        contains(&seen, b"hello from lad"),
        "missing expected output; got {:?}",
        String::from_utf8_lossy(&seen)
    );

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn two_daemons_with_distinct_versions_coexist() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let make = |tag: &'static str, tempdir_path: PathBuf| async move {
        let runtime_dir = tempdir_path.join(format!("runtime-{tag}"));
        let state_dir = tempdir_path.join(format!("state-{tag}"));
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        let socket = runtime_dir.join(format!("lad-{tag}.sock"));
        let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
        adapters.insert(
            "shtest".to_string(),
            Arc::new(ShellAdapter {
                script: "sleep 1".into(),
            }) as Arc<dyn AgentAdapter>,
        );
        let config = DaemonConfig {
            state_dir,
            socket_discovery: SocketDiscovery::with_override(socket.clone()),
            adapters,
            ..DaemonConfig::default()
        };
        let daemon = Daemon::bind(config).await.expect("bind");
        let (handle, join) = daemon.spawn();
        (socket, handle, join)
    };
    let (sock_a, handle_a, join_a) = make("1", tempdir.path().to_path_buf()).await;
    let (sock_b, handle_b, join_b) = make("2", tempdir.path().to_path_buf()).await;

    // Both sockets are reachable independently.
    let stream_a = connect(&Endpoint::uds(&sock_a)).await.expect("connect a");
    let stream_b = connect(&Endpoint::uds(&sock_b)).await.expect("connect b");
    drop(stream_a);
    drop(stream_b);

    handle_a.shutdown();
    handle_b.shutdown();
    let _ = timeout(Duration::from_secs(15), join_a).await;
    let _ = timeout(Duration::from_secs(15), join_b).await;
    assert!(!sock_a.exists(), "socket a should be unlinked on shutdown");
    assert!(!sock_b.exists(), "socket b should be unlinked on shutdown");
}

#[tokio::test]
async fn shutdown_signals_live_sessions_within_deadline() {
    let project = tempfile::tempdir().expect("project tmp");
    // Long-running child — we want to confirm shutdown actually terminates it.
    let daemon = bootstrap_daemon("trap 'exit 0' TERM; sleep 60").await;
    let mut conn = client(&daemon.socket).await;

    let create_params = SessionsCreateParams {
        project_dir: project.path().to_string_lossy().to_string(),
        backend: "shtest".to_string(),
        args: vec![],
        prompt: None,
        worktree: false,
    };
    send_request(&mut conn, 1, "sessions.create", &create_params).await;
    let result: SessionsCreateResult =
        serde_json::from_value(recv_response_for(&mut conn, 1).await).expect("decode");
    let _ = result.session_id;

    // Trigger graceful shutdown and confirm we return within the deadline.
    let started = std::time::Instant::now();
    daemon.handle.shutdown();
    let result = timeout(Duration::from_secs(12), daemon.join).await;
    assert!(result.is_ok(), "daemon shutdown didn't finish in time");
    assert!(
        started.elapsed() <= Duration::from_secs(12),
        "shutdown exceeded the 10s+ grace window: {:?}",
        started.elapsed()
    );
}

/// §6.4 hard-cap regression: a child that *ignores* SIGTERM still has
/// to be cleaned up via SIGKILL within the documented 10 s ceiling.
///
/// Without the single-deadline fix (PR review feedback) this test would
/// fail because the daemon ran two sequential 10 s windows — one for
/// connection drain, one for session drain — so the SIGKILL escalation
/// landed at ~20 s instead of within the §6.4 budget.
#[tokio::test]
async fn shutdown_kills_term_ignoring_child_within_hard_cap() {
    let project = tempfile::tempdir().expect("project tmp");
    // `trap '' TERM` permanently ignores SIGTERM; only SIGKILL stops it.
    // Sleep long enough that there is no race with natural exit.
    let daemon = bootstrap_daemon("trap '' TERM; sleep 120").await;
    let mut conn = client(&daemon.socket).await;

    let create_params = SessionsCreateParams {
        project_dir: project.path().to_string_lossy().to_string(),
        backend: "shtest".to_string(),
        args: vec![],
        prompt: None,
        worktree: false,
    };
    send_request(&mut conn, 1, "sessions.create", &create_params).await;
    let result: SessionsCreateResult =
        serde_json::from_value(recv_response_for(&mut conn, 1).await).expect("decode");
    let _ = result.session_id;

    // Give the child a beat to install the signal mask before we ask
    // the daemon to shut down — otherwise we might race and SIGTERM the
    // shell before `trap` finishes parsing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    daemon.handle.shutdown();
    let joined = timeout(Duration::from_secs(11), daemon.join).await;
    let elapsed = started.elapsed();
    assert!(
        joined.is_ok(),
        "daemon shutdown didn't finish within §6.4's 10s+epsilon (elapsed {elapsed:?})"
    );
    assert!(
        elapsed <= Duration::from_secs(11),
        "SIGKILL escalation must happen inside the 10s ceiling, took {elapsed:?}"
    );
}

/// `lad daemonize` end-to-end: spawn the actual binary, confirm the
/// socket appears, then send SIGTERM and confirm cleanup.
///
/// Requires the `lad` binary to already be built. Run with:
/// ```sh
/// LAD_BIN=$(cargo build -p la-daemon --bin lad 2>&1 \
///   && echo target/debug/lad) cargo test \
///   -p la-daemon --test acceptance lad_daemonize_binary_smoke
/// ```
#[test]
fn lad_daemonize_binary_smoke() {
    let Some(lad) = std::env::var_os("LAD_BIN") else {
        eprintln!("LAD_BIN unset; skipping daemonize binary smoke");
        return;
    };
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime = tempdir.path().join("runtime");
    let state = tempdir.path().join("state");
    let socket = runtime.join("lad-1.sock");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    let output = std::process::Command::new(&lad)
        .arg("daemonize")
        .arg("--socket")
        .arg(&socket)
        .arg("--state-dir")
        .arg(&state)
        .env("LAZYAGENTS_RUNTIME_DIR", &runtime)
        .output()
        .expect("spawn lad");
    assert!(
        output.status.success(),
        "lad daemonize failed: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(socket.exists(), "socket did not appear after daemonize");

    // Pull the pid out of `pid=N socket=…` so we can SIGTERM it cleanly.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pid: i32 = stdout
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("pid=").and_then(|p| p.parse().ok()))
        .expect("pid in output");

    // SIGTERM and wait for the socket to disappear.
    #[cfg(unix)]
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        assert_eq!(kill(pid, SIGTERM), 0, "kill returned non-zero");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if !socket.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("socket still present 15s after SIGTERM");
}

/// WEK-49 acceptance: a reconnecting client passes `resume_from_seq` and
/// gets a single `sessions.attach` RPC that both resubscribes and catches
/// up — no follow-up `sessions.replay` required. This is the regression
/// the M1.9 review (Blocker 1) called out: previously the daemon ignored
/// the resume hint (`manager.attach(&id, None, ...)`) so a fresh attach
/// returned all ring chunks again, double-delivering everything the
/// client had already seen.
///
/// Walk:
///   1. Start session that streams a deterministic, line-numbered output.
///   2. Attach with `resume_from_seq=None`; drain N frames; remember
///      `last_seq` and the bytes seen.
///   3. Detach. Bytes keep accumulating in the hub's ring.
///   4. Reattach on a brand-new connection with
///      `resume_from_seq=Some(last_seq)`.
///   5. Drain catch-up frames. Assert:
///      - every `seq` is strictly greater than `last_seq`,
///      - `seq` values are contiguous (no gap),
///      - no `session.gap` notification arrives,
///      - the catch-up bytes pick up where the first attach left off.
#[tokio::test]
async fn reattach_with_resume_from_seq_catches_up_without_double_delivery() {
    let project = tempfile::tempdir().expect("project tmp");
    // Each line is short enough that one `printf` ≈ one chunk; the
    // 0.05 s spacing keeps the test under a few seconds while still
    // giving the daemon time to push between us draining and reattaching.
    // 30 lines is large enough that we will detach midstream and still
    // have meaningful catch-up work to do on reattach.
    let script = "for i in $(seq 1 30); do printf 'line-%02d\\n' $i; sleep 0.05; done";
    let daemon = bootstrap_daemon(script).await;
    let mut conn = client(&daemon.socket).await;

    // sessions.create
    let create_params = SessionsCreateParams {
        project_dir: project.path().to_string_lossy().to_string(),
        backend: "shtest".to_string(),
        args: vec![],
        prompt: None,
        worktree: false,
    };
    send_request(&mut conn, 1, "sessions.create", &create_params).await;
    let result: SessionsCreateResult =
        serde_json::from_value(recv_response_for(&mut conn, 1).await).expect("decode create");
    let session_id = result.session_id;

    // First attach: no resume token, take whatever the daemon has so far.
    let attach_params = SessionsAttachParams {
        session_id: session_id.clone(),
        resume_from_seq: None,
        replay_bytes: None,
        acquire_input: false,
    };
    send_request(&mut conn, 2, "sessions.attach", &attach_params).await;
    let _attach: SessionsAttachResult =
        serde_json::from_value(recv_response_for(&mut conn, 2).await).expect("decode attach");

    // Drain until we've seen the marker for line 5; remember the last seq.
    let mut first_bytes = Vec::<u8>::new();
    let mut first_seqs = Vec::<u64>::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !contains(&first_bytes, b"line-05") {
        let msg = match tokio::time::timeout(Duration::from_millis(200), conn.recv()).await {
            Ok(Ok(Some(m))) => m,
            _ => continue,
        };
        if let Message::Notification(n) = msg {
            if n.method == "session.output" {
                if let Some(params) = n.params.as_ref() {
                    let p: SessionOutputParams =
                        serde_json::from_value(params.clone()).expect("decode output");
                    first_seqs.push(p.seq);
                    first_bytes.extend_from_slice(&p.data_bytes().expect("base64"));
                }
            } else if n.method == "session.gap" {
                panic!("unexpected gap during first attach drain: {n:?}");
            }
        }
    }
    assert!(
        contains(&first_bytes, b"line-05"),
        "first attach never saw line 5; got {:?}",
        String::from_utf8_lossy(&first_bytes)
    );
    let last_seq = *first_seqs.last().expect("first attach saw no chunks");

    // sessions.detach — and drop the connection so the daemon eagerly
    // parks the subscription. We do NOT wait for park eviction; a fresh
    // attach creates a fresh subscription either way.
    send_request(
        &mut conn,
        3,
        "sessions.detach",
        &SessionsDetachParams {
            session_id: session_id.clone(),
        },
    )
    .await;
    let _: SessionsDetachResult =
        serde_json::from_value(recv_response_for(&mut conn, 3).await).expect("decode detach");
    drop(conn);

    // Let the script emit more lines while we're "away" so the catch-up
    // path has real work to do. 200 ms × 20 lines-per-second ≈ 4 lines.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Reconnect on a brand-new socket. New conn, new client_id.
    let mut conn2 = client(&daemon.socket).await;
    let attach_resume = SessionsAttachParams {
        session_id: session_id.clone(),
        resume_from_seq: Some(last_seq),
        replay_bytes: None,
        acquire_input: false,
    };
    send_request(&mut conn2, 10, "sessions.attach", &attach_resume).await;
    let resume_ack: SessionsAttachResult =
        serde_json::from_value(recv_response_for(&mut conn2, 10).await).expect("decode reattach");
    assert!(
        resume_ack.snapshot_seq >= last_seq,
        "snapshot_seq ({}) must cover at least last_seq ({last_seq})",
        resume_ack.snapshot_seq
    );

    // Drain until we've seen the marker for line 25 (well past the
    // detach point) or the script ends. Track every seq and the catch-up
    // bytes; assert no gap, no replay of already-seen seqs.
    let mut catchup_bytes = Vec::<u8>::new();
    let mut catchup_seqs = Vec::<u64>::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline && !contains(&catchup_bytes, b"line-25") {
        let msg = match tokio::time::timeout(Duration::from_millis(300), conn2.recv()).await {
            Ok(Ok(Some(m))) => m,
            _ => continue,
        };
        if let Message::Notification(n) = msg {
            if n.method == "session.output" {
                if let Some(params) = n.params.as_ref() {
                    let p: SessionOutputParams =
                        serde_json::from_value(params.clone()).expect("decode output");
                    catchup_seqs.push(p.seq);
                    catchup_bytes.extend_from_slice(&p.data_bytes().expect("base64"));
                }
            } else if n.method == "session.gap" {
                panic!("WEK-49 regression: catch-up after resume_from_seq must not emit a gap (got {n:?})");
            }
        }
    }
    assert!(
        contains(&catchup_bytes, b"line-25"),
        "reattach never saw line 25; catchup bytes = {:?}",
        String::from_utf8_lossy(&catchup_bytes)
    );

    // Core regression check: every catch-up seq must be strictly greater
    // than what the first attach already drained. If the daemon were
    // still hardcoding `None` (the pre-WEK-49 bug) we would see
    // `first_seqs` repeated here.
    let first_seen: std::collections::BTreeSet<u64> = first_seqs.iter().copied().collect();
    for s in &catchup_seqs {
        assert!(
            *s > last_seq,
            "catch-up returned seq {s} which is <= last_seq {last_seq}; \
             resume_from_seq is being ignored",
        );
        assert!(
            !first_seen.contains(s),
            "catch-up double-delivered seq {s} that the first attach already drained",
        );
    }

    // Contiguity: seqs must increase by exactly 1 inside the catch-up
    // window (and the first one must be last_seq + 1).
    assert_eq!(
        catchup_seqs.first().copied(),
        Some(last_seq + 1),
        "first catch-up seq must be last_seq+1 (got {:?}, last_seq={last_seq})",
        catchup_seqs.first()
    );
    for w in catchup_seqs.windows(2) {
        assert_eq!(
            w[1],
            w[0] + 1,
            "catch-up seqs must be contiguous: {:?} → {:?}",
            w[0],
            w[1]
        );
    }

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}
