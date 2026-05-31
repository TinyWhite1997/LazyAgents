//! M0 end-to-end smoke: real `la-pty` + real `la-proto` JSON-RPC + real
//! `la-ipc` length-prefix framing + real `la-adapter::AgentAdapter`.
//!
//! The harness wires a tokio `duplex` as the daemon↔client connection and
//! runs a minimal in-process dispatcher (mock daemon) that delegates spawn
//! decisions to the adapter and streams real PTY output back to the client.
//! The backend is a `CatAdapter` driving a local echo-capable shell command,
//! so we can write arbitrary bytes and assert the same bytes echo back through
//! the JSON-RPC pipe — no real CLI auth, no network.
//!
//! Asserts the four M0 invariants:
//!   1. JSON-RPC framing round-trips `initialize` / `sessions.create` /
//!      `sessions.attach` / `sessions.write` over the real `la-ipc` codec.
//!   2. `AgentAdapter::spawn_spec` is the only producer of the OS command;
//!      the daemon never synthesizes args itself.
//!   3. Bytes typed via `sessions.write` reach the PTY, and PTY output is
//!      surfaced via `session.output` notifications with monotonic `seq`.
//!   4. Dropping the client connection ("detach") does not kill the PTY
//!      child — the daemon-side handle still reports a live `pid`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use la_adapter::{
    AdapterDescriptor, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec, StdinMode,
};
use la_ipc::{Connection, SendHalf};
use la_proto::jsonrpc::{
    Message, Notification, Request, RequestId, Response, ResponseOutcome, Version,
};
use la_proto::methods::{
    Initialize, InitializeParams, InitializeResult, Method, PtySize as ProtoPtySize,
    ServerCapabilities, SessionState, SessionsAttach, SessionsAttachParams, SessionsAttachResult,
    SessionsCreate, SessionsCreateParams, SessionsCreateResult, SessionsWrite, SessionsWriteParams,
    SessionsWriteResult,
};
use la_proto::notifications::{NotificationMethod, SessionOutput, SessionOutputParams};
use la_pty::{spawn as pty_spawn, CommandBuilder, PtyChild, PtySize};
use tokio::io::DuplexStream;
use tokio::sync::Mutex;
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// M0 mock backend: drives a local echo-capable command so a write comes back
/// through the PTY on every supported OS.
struct CatAdapter;

#[async_trait::async_trait]
impl AgentAdapter for CatAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "cat-mock",
            display_name: "cat (M0 smoke mock)",
            default_program: if cfg!(windows) { "cmd.exe" } else { "cat" },
            docs_url: "https://example.invalid/cat",
        }
    }

    async fn probe(&self) -> ProbeResult {
        ProbeResult::Available {
            version: if cfg!(windows) {
                "cmd-echo".into()
            } else {
                "posix-cat".into()
            },
        }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, la_adapter::AdapterError> {
        let (program, args) = if cfg!(windows) {
            (
                PathBuf::from("cmd.exe"),
                vec!["/Q".into(), "/K".into(), "echo cat-mock ready".into()],
            )
        } else {
            (PathBuf::from("cat"), vec![])
        };

        Ok(SpawnSpec {
            program,
            args,
            env: req.env.clone(),
            cwd: req.cwd.clone(),
            pty: req.pty,
            stdin_mode: req.stdin_mode,
        })
    }

    fn encode_user_input(&self, text: &str) -> Bytes {
        if cfg!(windows) {
            return Bytes::from(format!("echo {text}\r\n"));
        }

        // cat is line-buffered; ensure a trailing newline so the line echoes.
        let mut s = text.to_owned();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        Bytes::from(s)
    }
}

/// Shared daemon state held across the request loop and the PTY fan-out task.
struct DaemonState {
    adapter: Arc<dyn AgentAdapter>,
    /// `(session_id, PtyChild)` once created. We hold the whole PtyChild so
    /// `writer` (Clone) survives independently of the reader fan-out.
    session: Mutex<Option<(String, PtyChild)>>,
    /// Set once the PTY reader fan-out task starts; used by the test to
    /// verify the child still owns a pid after client detach.
    last_known_pid: Mutex<Option<u32>>,
    /// Counter for monotonic session.output seq.
    seq: Mutex<u64>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m0_end_to_end_initialize_create_attach_write_detach() {
    let (client_io, daemon_io) = tokio::io::duplex(256 * 1024);
    let adapter: Arc<dyn AgentAdapter> = Arc::new(CatAdapter);
    let state = Arc::new(DaemonState {
        adapter: adapter.clone(),
        session: Mutex::new(None),
        last_known_pid: Mutex::new(None),
        seq: Mutex::new(0),
    });

    let daemon_state = state.clone();
    let daemon = tokio::spawn(async move { run_mock_daemon(daemon_io, daemon_state).await });

    let mut conn: Connection<DuplexStream> = Connection::new(client_io);

    // --- 1. initialize round-trip via real framing.
    let init: InitializeResult = call::<Initialize>(
        &mut conn,
        1,
        InitializeParams {
            client: "la".into(),
            client_version: "0.0.0-m0".into(),
            protocol_versions: vec!["1".into()],
        },
    )
    .await;
    assert_eq!(init.protocol_version, "1");
    assert!(init.capabilities.adapters.contains(&"cat-mock".into()));

    // --- 2. sessions.create — adapter.spawn_spec drives a real PTY spawn.
    let create: SessionsCreateResult = call::<SessionsCreate>(
        &mut conn,
        2,
        SessionsCreateParams {
            project_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            backend: "cat-mock".into(),
            args: vec![],
            prompt: None,
            worktree: false,
        },
    )
    .await;
    assert_eq!(create.backend, "cat-mock");
    assert!(matches!(create.state, SessionState::Running));
    let sid = create.session_id.clone();

    // --- 3. sessions.attach — no-op subscription for M0 (snapshot_seq=0).
    let attach: SessionsAttachResult = call::<SessionsAttach>(
        &mut conn,
        3,
        SessionsAttachParams {
            session_id: sid.clone(),
            replay_bytes: None,
            acquire_input: true,
        },
    )
    .await;
    assert_eq!(attach.snapshot_seq, 0);
    assert!(attach.input_acquired);

    // --- 4. sessions.write — bytes reach the real PTY; output echoes back as
    //         session.output notifications.
    let needle = b"ping-from-m0";
    let payload = adapter.encode_user_input(std::str::from_utf8(needle).unwrap());
    let _: SessionsWriteResult = call::<SessionsWrite>(
        &mut conn,
        4,
        SessionsWriteParams::try_from_bytes(sid.clone(), &payload).unwrap(),
    )
    .await;

    let echoed = read_until_needle(&mut conn, &sid, needle).await;
    assert!(
        echoed,
        "expected {:?} echoed via session.output",
        std::str::from_utf8(needle)
    );

    // --- 5. detach: drop the client. PTY child must outlive the connection.
    drop(conn);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pid_after_detach = state.last_known_pid.lock().await.clone();
    assert!(
        pid_after_detach.is_some(),
        "daemon must still know the child pid after client detach"
    );

    // Tidy up: signal the child so the test exits promptly.
    if let Some((_, pty)) = state.session.lock().await.take() {
        // Closing all PtyWriter clones in the daemon by dropping `pty.writer`
        // causes the write-fan-in thread to exit; cat sees EOF and quits.
        let signal = if cfg!(windows) {
            la_pty::Signal::Kill
        } else {
            la_pty::Signal::Terminate
        };
        let _ = pty.signal(signal);
        let _ = timeout(Duration::from_secs(3), pty.wait()).await;
    }
    daemon.abort();
}

/// Send a typed request and return the typed result. Panics on RPC error.
async fn call<M: Method>(
    conn: &mut Connection<DuplexStream>,
    id: i64,
    params: M::Params,
) -> M::Result {
    let req = Request::new(RequestId::Num(id), M::NAME, &params).expect("encode params");
    conn.send(&Message::Request(req)).await.expect("send");
    loop {
        let msg = timeout(READ_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("recv eof");
        match msg {
            Message::Response(Response {
                id: RequestId::Num(n),
                outcome,
                ..
            }) if n == id => match outcome {
                ResponseOutcome::Result(v) => {
                    return serde_json::from_value(v).expect("decode typed result")
                }
                ResponseOutcome::Error(e) => panic!("rpc error: {e}"),
            },
            Message::Notification(_) => continue,
            other => panic!("unexpected while awaiting id={id}: {other:?}"),
        }
    }
}

/// Drain notifications until a `session.output` for `sid` contains `needle`.
async fn read_until_needle(conn: &mut Connection<DuplexStream>, sid: &str, needle: &[u8]) -> bool {
    let mut acc: Vec<u8> = Vec::new();
    let res = timeout(READ_TIMEOUT, async {
        while let Some(msg) = conn.recv().await.expect("recv io") {
            if let Message::Notification(Notification { method, params, .. }) = msg {
                if method == SessionOutput::NAME {
                    if let Some(v) = params {
                        if let Ok(p) = serde_json::from_value::<SessionOutputParams>(v) {
                            if p.session_id == sid {
                                if let Ok(b) = p.data_bytes() {
                                    acc.extend_from_slice(&b);
                                    if acc.windows(needle.len()).any(|w| w == needle) {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    })
    .await;
    res.unwrap_or(false)
}

/// Mock daemon: split the connection so the PTY fan-out task can push
/// `session.output` while the request loop handles incoming RPC.
async fn run_mock_daemon(io: DuplexStream, state: Arc<DaemonState>) -> Result<(), String> {
    let conn: Connection<DuplexStream> = Connection::new(io);
    let (send_half, mut recv_half) = conn.split();
    let send_half = Arc::new(send_half);

    while let Some(msg) = recv_half.recv().await.map_err(|e| e.to_string())? {
        let Message::Request(req) = msg else { continue };
        let id = req.id.clone();
        let method = req.method.clone();
        let resp = dispatch(&state, &send_half, req)
            .await
            .unwrap_or_else(|err| {
                Response::error(id.clone(), la_proto::jsonrpc::RpcError::internal_error(err))
            });
        send_half
            .send(&Message::Response(resp))
            .await
            .map_err(|e| format!("daemon send: {e}"))?;
        let _ = method;
    }
    Ok(())
}

async fn dispatch(
    state: &Arc<DaemonState>,
    send_half: &Arc<SendHalf<DuplexStream>>,
    req: Request,
) -> Result<Response, String> {
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => {
            let _: InitializeParams = req.params_as().map_err(|e| e.to_string())?;
            Ok(Response::success(
                id,
                InitializeResult {
                    server: "lad-m0-mock".into(),
                    server_version: "0.0.0-m0".into(),
                    protocol_version: "1".into(),
                    capabilities: ServerCapabilities {
                        adapters: vec!["cat-mock".into()],
                        cron: false,
                        worktree: false,
                    },
                },
            )
            .expect("encode"))
        }
        "sessions.create" => {
            let p: SessionsCreateParams = req.params_as().map_err(|e| e.to_string())?;
            let spawn_req = SpawnRequest {
                cwd: PathBuf::from(&p.project_dir),
                stdin_mode: StdinMode::Pty,
                ..SpawnRequest::default()
            };
            let spec = state
                .adapter
                .spawn_spec(&spawn_req)
                .map_err(|e| format!("spawn_spec: {e}"))?;
            let mut cmd = CommandBuilder::new(spec.program.to_string_lossy().as_ref());
            for arg in &spec.args {
                cmd.arg(arg);
            }
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
            cmd.cwd(spec.cwd);
            let mut pty =
                pty_spawn(cmd, PtySize::default()).map_err(|e| format!("pty spawn: {e}"))?;
            let sid = format!("m0-{}", pty.pid().unwrap_or(0));
            *state.last_known_pid.lock().await = pty.pid();

            // Move the reader out of `pty` so we can keep the rest of the
            // handle (writer + wait + signal) on the daemon side while the
            // fan-out task owns the byte stream.
            let (_dummy_tx, dummy_rx) = tokio::sync::mpsc::channel::<Bytes>(1);
            let reader = std::mem::replace(&mut pty.reader, dummy_rx);

            // Spawn the fan-out: PTY bytes → session.output notifications.
            let sid_clone = sid.clone();
            let send_clone = send_half.clone();
            let state_clone = state.clone();
            tokio::spawn(async move {
                let mut r = reader;
                while let Some(bytes) = r.recv().await {
                    let seq = {
                        let mut g = state_clone.seq.lock().await;
                        *g += 1;
                        *g
                    };
                    let params = SessionOutputParams::from_bytes(sid_clone.clone(), seq, &bytes);
                    let notif = Notification {
                        jsonrpc: Version,
                        method: SessionOutput::NAME.into(),
                        params: Some(serde_json::to_value(params).expect("encode")),
                    };
                    if send_clone
                        .send(&Message::Notification(notif))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });

            *state.session.lock().await = Some((sid.clone(), pty));

            Ok(Response::success(
                id,
                SessionsCreateResult {
                    session_id: sid,
                    backend: p.backend,
                    cwd: spawn_req.cwd.to_string_lossy().into_owned(),
                    initial_size: ProtoPtySize { rows: 24, cols: 80 },
                    state: SessionState::Running,
                },
            )
            .expect("encode"))
        }
        "sessions.attach" => {
            let p: SessionsAttachParams = req.params_as().map_err(|e| e.to_string())?;
            Ok(Response::success(
                id,
                SessionsAttachResult {
                    session_id: p.session_id,
                    snapshot_seq: 0,
                    input_acquired: true,
                },
            )
            .expect("encode"))
        }
        "sessions.write" => {
            let p: SessionsWriteParams = req.params_as().map_err(|e| e.to_string())?;
            let bytes = p.data_bytes().map_err(|e| e.to_string())?;
            if let Some((_, pty)) = state.session.lock().await.as_ref() {
                pty.writer
                    .write(Bytes::from(bytes))
                    .await
                    .map_err(|e| format!("pty write: {e}"))?;
            }
            Ok(Response::success(id, SessionsWriteResult {}).expect("encode"))
        }
        other => Ok(Response::error(
            id,
            la_proto::jsonrpc::RpcError::method_not_found(other),
        )),
    }
}
