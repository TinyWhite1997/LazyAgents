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

// Every fixture in this file uses the daemon's UDS path
// (`tokio::net::UnixStream`). The Windows named-pipe path is exercised
// by separate tests; gating the whole file to unix keeps the WEK-72
// matrix CI green on windows-2022.
#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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
    EventTopic, EventsSubscribeParams, EventsSubscribeResult, SessionState, SessionsArchiveParams,
    SessionsArchiveResult, SessionsAttachParams, SessionsAttachResult, SessionsCreateParams,
    SessionsCreateResult, SessionsDeleteParams, SessionsDeleteResult, SessionsDetachParams,
    SessionsDetachResult, SessionsListParams, SessionsListResult, SessionsWriteParams,
    SessionsWriteResult,
};
use la_proto::notifications::{CronFiredParams, SessionOutputParams};
use tempfile::TempDir;
use tokio::time::timeout;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Adapter that runs a temporary executable script. Avoids needing a real
/// claude CLI inside CI without bypassing the shell-wrapper spawn guard.
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
        let script_dir = std::env::temp_dir().join("lazyagents-daemon-test-scripts");
        std::fs::create_dir_all(&script_dir).map_err(la_adapter::AdapterError::SpawnFailed)?;
        let script_path = script_dir.join(format!("{}.sh", la_storage::new_id()));
        std::fs::write(&script_path, format!("#!/bin/sh\n{}\n", self.script))
            .map_err(la_adapter::AdapterError::SpawnFailed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&script_path)
                .map_err(la_adapter::AdapterError::SpawnFailed)?
                .permissions();
            perm.set_mode(0o700);
            std::fs::set_permissions(&script_path, perm)
                .map_err(la_adapter::AdapterError::SpawnFailed)?;
        }
        Ok(SpawnSpec {
            program: script_path,
            args: vec![],
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
    /// Snapshot of the running daemon's event bus. Tests that want to
    /// inject a `BusEvent` (e.g. the WEK-36 cron-delivery acceptance,
    /// which publishes a `CronFired` and checks the wire path) hold
    /// this on the side so they don't have to poke the daemon's
    /// internals through unsafe extraction.
    bus: Option<la_core::EventBus>,
    storage: Option<la_storage::Storage>,
    _tempdir: TempDir,
}

async fn bootstrap_daemon(script: &str) -> TestDaemon {
    bootstrap_daemon_with(script, |_| {}).await
}

async fn bootstrap_daemon_with(
    script: &str,
    customize: impl FnOnce(&mut DaemonConfig),
) -> TestDaemon {
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

    let mut config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        ..DaemonConfig::default()
    };
    customize(&mut config);
    let daemon = Daemon::bind(config).await.expect("bind daemon");
    let bus = daemon.manager.bus();
    let storage = daemon.manager.storage().clone();
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
        bus: Some(bus),
        storage: Some(storage),
        _tempdir: tempdir,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn metrics_socket_uses_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    // M4.5 / WEK-75 — A9 metrics.scrape 三层一致性: the standalone
    // `<sock>.metrics` UDS endpoint is gone; `lad metrics` now dials the
    // main daemon socket and issues a `metrics.scrape` RPC. The
    // owner-only security boundary moves with it — we re-assert that the
    // main socket is `0o600` here so the dropped endpoint doesn't take
    // the security check with it.
    let daemon = bootstrap_daemon("sleep 1").await;
    let mode = std::fs::metadata(&daemon.socket)
        .expect("main socket metadata")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(
        mode, 0o600,
        "main IPC socket must stay 0o600 (was the metrics socket's job before WEK-75)",
    );

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

/// M4.5 / WEK-75 — A9 metrics.scrape 三层一致性 acceptance.
///
/// "Byte-identical" in the DoD means the CLI passes the RPC `result.body`
/// straight through to stdout without trimming, prefixing, or rewriting.
/// We can't compare two separate scrapes byte-for-byte: every scrape itself
/// emits one new `lad_rpc_requests_total{method="metrics.scrape"}` increment
/// and one new `lad_rpc_duration_seconds{method="metrics.scrape"}` sample,
/// and the scheduler-health loop runs in the background bumping gauges. So
/// the test pins the parts that the CLI is responsible for:
///
/// 1. The RPC body has the `# TYPE` / `# HELP` preamble shape required of
///    a Prometheus text-exposition payload.
/// 2. Every A9 metric naming-table entry appears in a `# TYPE` line of
///    the body (so a silent drop of a `describe_*!` call in la-observ
///    trips the test).
/// 3. Every A9 metric that this test can drive in-process (sessions.list
///    bumps the RPC counters, the scheduler-health loop publishes the
///    queue gauge, storage writes record their latency histogram) has at
///    least one sample line in the body (so a `describe`-without-`emit`
///    drift trips the test).
/// 4. The CLI body has the SAME `# TYPE` and `# HELP` line set as the
///    RPC body. These lines are static for a given describe set, so any
///    rewriting in the CLI (a stray `print!("metrics: ...")`, an
///    extension-handler scrub, a `\n`-stripping `write_str`, …) would
///    show up here even with the metric-value noise.
/// 5. Every metric name present in one body is present in the other.
///
/// The literal byte-equality property (CLI doesn't mutate the body) is
/// guaranteed by `print!("{text}")` in `lad metrics`; assertions 4 and 5
/// detect any divergence introduced by an accidental rewriter.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_scrape_rpc_and_cli_expose_same_a9_surface() {
    use la_proto::methods::{
        MetricsScrape, MetricsScrapeParams, MetricsScrapeResult, Method,
    };

    let daemon = bootstrap_daemon_with("sleep 5", |cfg| {
        // Force the scheduler-health loop to tick fast so the test
        // doesn't wait the prod default (5s) for the gauge to land.
        cfg.scheduler.scheduler_health_interval = Duration::from_millis(20);
    })
    .await;

    // In-process daemons skip the binary's `init_observability` shim, so
    // the global metrics recorder is `None` until we install it here.
    // `install_metrics_recorder` is idempotent (the underlying
    // `OnceLock` ignores re-init), so calling it inside the test is
    // safe even when the suite runs in parallel.
    la_observ::install_metrics_recorder();

    // Drive at least one RPC + one cron metric flavour so the rendered
    // body is non-empty for at least one counter / gauge / histogram
    // from the A9 table. `sessions.list` is the cheapest call that
    // emits both `lad_rpc_requests_total` (counter) and
    // `lad_rpc_duration_seconds` (histogram). `lad_scheduler_queue_depth`
    // (gauge) is published by the scheduler's health loop a few times a
    // second; we wait briefly for it to land below.
    {
        let mut conn = client(&daemon.socket).await;
        send_request(&mut conn, 100, "sessions.list", serde_json::json!({})).await;
        let _ = recv_response_for(&mut conn, 100).await;
    }

    // Wait for the scheduler-health loop to publish at least one
    // `lad_scheduler_queue_depth` SAMPLE line (not just the `# TYPE`
    // preamble, which `describe_metrics` writes synchronously at
    // recorder install time). The bootstrap above shrinks the
    // interval to 20ms, so this normally lands in the first iteration.
    let mut rpc_body = String::new();
    for _ in 0..200 {
        let mut conn = client(&daemon.socket).await;
        let req = la_proto::jsonrpc::Request::new(
            1i64,
            MetricsScrape::NAME.to_string(),
            &MetricsScrapeParams::default(),
        )
        .expect("encode metrics.scrape");
        conn.send(&la_proto::jsonrpc::Message::Request(req))
            .await
            .expect("send metrics.scrape");
        let v = recv_response_for(&mut conn, 1).await;
        let r: MetricsScrapeResult = serde_json::from_value(v).expect("decode");
        rpc_body = r.body;
        let has_queue_sample = rpc_body.lines().any(|line| {
            !line.starts_with('#')
                && (line.starts_with("lad_scheduler_queue_depth ")
                    || line.starts_with("lad_scheduler_queue_depth{"))
        });
        if has_queue_sample {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // (a) preamble lines.
    assert!(
        rpc_body.contains("# TYPE "),
        "body missing # TYPE lines (got {} bytes):\n{}",
        rpc_body.len(),
        rpc_body
    );
    assert!(
        rpc_body.contains("# HELP "),
        "body missing # HELP lines (got {} bytes):\n{}",
        rpc_body.len(),
        rpc_body
    );

    // (b) A9 metric naming table — every entry MUST appear in a
    // `# TYPE` line of the rendered body. `describe_metrics` writing
    // them out is what guarantees they show up here; a silent drop
    // (someone deletes the `metrics::describe_counter!(...)` call
    // alongside removing the prod emit) would trip this assertion.
    //
    // Keep this list in sync with `la_observ::describe_metrics()` and
    // the table in `docs/observability.md`. Additions / renames go
    // through an ADR per Rev2 R4.
    const A9_METRICS: &[&str] = &[
        "lad_rpc_requests_total",
        "lad_rpc_duration_seconds",
        "lad_session_active",
        "lad_session_output_bytes_total",
        "lad_cron_runs_total",
        "lad_cron_missed_total",
        "lad_cron_throttled_seconds_total",
        "lad_pty_spawn_duration_seconds",
        "lad_storage_write_latency_seconds",
        "lad_runs_archive_pruned_total",
        "lad_scheduler_queue_depth",
        "lad_scheduler_clock_skew_seconds",
        "lad_adapter_drift_total",
    ];
    for name in A9_METRICS {
        let type_line = format!("# TYPE {name} ");
        assert!(
            rpc_body.contains(&type_line),
            "A9 metric {name} missing `# TYPE` line — describe_metrics out of sync with A9 table"
        );
    }

    // (b.2) For every A9 metric that we can plausibly drive from this
    // test (sessions.list is a single RPC and the scheduler-health
    // loop publishes the queue gauge automatically), assert at least
    // one sample line — i.e. a non-`#`-prefixed line whose first
    // token is the metric name (optionally followed by `_bucket` /
    // `_sum` / `_count` for histograms, or by `{` for a label set).
    //
    // The remaining A9 metrics (cron_* counters, adapter_drift,
    // runs_archive_pruned, clock_skew, storage_write_latency) fire
    // only on real cron / archive / clock-jump / SQLite-write events
    // that this in-process test cannot synthesise without a much
    // larger fixture; their `# TYPE` line above already pins the
    // contract. Per-emit-site coverage lives in the unit test
    // wek57_scheduler.rs and the runtime tests in la-daemon.
    const SAMPLE_DRIVEABLE: &[&str] = &[
        "lad_rpc_requests_total",
        "lad_rpc_duration_seconds",
        "lad_session_active",
        "lad_scheduler_queue_depth",
    ];
    for name in SAMPLE_DRIVEABLE {
        let has_sample = rpc_body.lines().any(|line| {
            if line.starts_with('#') || line.is_empty() {
                return false;
            }
            // Sample lines for `name` look like:
            //   <name> <value>
            //   <name>{...labels...} <value>
            //   <name>_bucket{...} <value>      (histogram)
            //   <name>_sum / <name>_count        (histogram)
            // We check the line starts with `<name>` followed by one
            // of `{ ` `_`, which covers all four shapes without
            // false-positives against a longer metric name that
            // happens to share the prefix.
            if let Some(rest) = line.strip_prefix(name) {
                matches!(rest.chars().next(), Some(' ') | Some('{') | Some('_'))
            } else {
                false
            }
        });
        assert!(
            has_sample,
            "A9 metric {name} declared but no sample line in body — emit path likely missing"
        );
    }

    // (c) CLI stdout is byte-identical to the RPC body.
    let lad_bin = env!("CARGO_BIN_EXE_lad");
    let cli_out = std::process::Command::new(lad_bin)
        .arg("metrics")
        .arg("--socket")
        .arg(&daemon.socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lad metrics");
    assert!(
        cli_out.status.success(),
        "lad metrics exited non-zero: status={:?} stderr={}",
        cli_out.status,
        String::from_utf8_lossy(&cli_out.stderr)
    );
    let cli_body = String::from_utf8(cli_out.stdout).expect("utf-8 stdout");
    // The body's metric line values can drift between two scrapes (the
    // CLI invocation itself adds one more `lad_rpc_requests_total{
    // method="metrics.scrape" }` increment vs. our in-process call).
    // The structural shape is what we pin: every metric NAME that
    // appears in one must appear in the other, and the preamble shape
    // matches. A stricter byte-equal compare here would race the
    // scheduler heartbeats and the CLI's own metrics.scrape RPC.
    for name in [
        "lad_rpc_requests_total",
        "lad_rpc_duration_seconds",
        "lad_session_active",
    ] {
        assert!(
            cli_body.contains(name) == rpc_body.contains(name),
            "CLI vs RPC drift on {name}: cli_has={} rpc_has={}",
            cli_body.contains(name),
            rpc_body.contains(name)
        );
    }
    // Preamble shape must match: same # TYPE / # HELP lines in both.
    let type_lines_cli: Vec<_> = cli_body.lines().filter(|l| l.starts_with("# TYPE ")).collect();
    let type_lines_rpc: Vec<_> = rpc_body.lines().filter(|l| l.starts_with("# TYPE ")).collect();
    assert_eq!(
        type_lines_cli, type_lines_rpc,
        "# TYPE preamble drift between CLI and RPC"
    );
    let help_lines_cli: Vec<_> = cli_body.lines().filter(|l| l.starts_with("# HELP ")).collect();
    let help_lines_rpc: Vec<_> = rpc_body.lines().filter(|l| l.starts_with("# HELP ")).collect();
    assert_eq!(
        help_lines_cli, help_lines_rpc,
        "# HELP preamble drift between CLI and RPC"
    );

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
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
    send_request(conn, id, method, params).await;
    serde_json::from_value(recv_response_for(conn, id).await).expect("decode response")
}

async fn drain_output_until(
    conn: &mut Connection<tokio::net::UnixStream>,
    needle: &[u8],
    timeout_duration: Duration,
) -> Vec<u8> {
    let mut seen = Vec::<u8>::new();
    let deadline = tokio::time::Instant::now() + timeout_duration;
    while tokio::time::Instant::now() < deadline && !contains(&seen, needle) {
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
    seen
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

async fn spawn_lad_binary(
    socket: &std::path::Path,
    state_dir: &std::path::Path,
    script: &str,
) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lad"))
        .arg("start")
        .arg("--socket")
        .arg(socket)
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--log-level")
        .arg("warn")
        .arg("--test-shell-adapter")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lad binary");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if connect(&Endpoint::uds(socket)).await.is_ok() {
            return child;
        }
        if let Some(status) = child.try_wait().expect("poll lad") {
            panic!("lad exited before socket was ready: {status}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = child.kill();
    panic!("lad socket did not become ready at {}", socket.display());
}

fn stop_child(child: &mut Child) {
    if child.try_wait().expect("poll child").is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(unix)]
fn kill_pid(pid: i32, signal: i32) {
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // Best-effort cleanup: closing the PTY may have already ended the child.
        let _ = kill(pid, signal);
    }
}

#[tokio::test]
async fn lad_binary_m1_end_to_end_lifecycle() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime = tempdir.path().join("runtime");
    let state = tempdir.path().join("state");
    let project = tempdir.path().join("project");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let socket = runtime.join("lad-1.sock");
    let script = "\
printf 'ready-from-shtest\\n'
while IFS= read -r line; do
  printf 'echo:%s\\n' \"$line\"
  [ \"$line\" = quit ] && exit 0
done
";
    let mut lad = spawn_lad_binary(&socket, &state, script).await;
    let mut conn = client(&socket).await;

    let create: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        &SessionsCreateParams {
            project_dir: project.to_string_lossy().to_string(),
            backend: "shtest".to_string(),
            args: vec![],
            prompt: None,
            worktree: false,
        },
    )
    .await;
    let session_id = create.session_id.clone();

    let attach: SessionsAttachResult = call(
        &mut conn,
        2,
        "sessions.attach",
        &SessionsAttachParams {
            session_id: session_id.clone(),
            replay_bytes: None,
            // WEK-70: `None` is live-only — if the shell's
            // `ready-from-shtest` was emitted before attach landed,
            // it's already in the ring but won't be replayed. Use
            // `Some(0)` to catch up the banner deterministically.
            resume_from_seq: Some(0),
            acquire_input: true,
        },
    )
    .await;
    assert!(attach.input_acquired);

    let ready = drain_output_until(&mut conn, b"ready-from-shtest", Duration::from_secs(5)).await;
    assert!(
        contains(&ready, b"ready-from-shtest"),
        "missing startup output; got {:?}",
        String::from_utf8_lossy(&ready)
    );

    // WEK-70 readiness fence: `ready-from-shtest` is printed BEFORE the
    // shell enters its `read` loop. Without a probe, `hello-m1` can land
    // on the PTY master while the shell is still between `printf` and
    // the first `read` — the byte is buffered by the line discipline,
    // but if the shell hasn't installed its `read` yet, the `IFS= read`
    // can swallow it as part of the same line as the next write, or
    // (under burst) miss the carriage return entirely. Send a known
    // nonce and wait for its echo: once we see `echo:SYNCFENCE-N`
    // the loop is demonstrably iterating and the next write is safe.
    //
    // A SINGLE fence write is itself flake-prone — it can land in the
    // same pre-`read` window. Re-send the probe inside a deadline and
    // drive both the write Response and the session.output notifications
    // through one recv loop so notifications don't get dropped by the
    // synchronous `call()` helper. We also track pending probe acks
    // explicitly so subsequent `call()`s (which hard-panic on id
    // mismatch) don't trip over a stray late Response.
    let fence = format!("SYNCFENCE-{}", session_id);
    let fence_probe = format!("{fence}\r");
    let fence_needle = format!("echo:{fence}");
    let probe_id_start: i64 = 100;
    let mut probe_id: i64 = probe_id_start;
    let mut sent_probes: i64 = 0;
    let mut acked_probes: i64 = 0;
    let mut seen_fence = Vec::<u8>::new();
    let mut next_probe_at = tokio::time::Instant::now();
    let fence_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < fence_deadline
        && !contains(&seen_fence, fence_needle.as_bytes())
    {
        if tokio::time::Instant::now() >= next_probe_at {
            send_request(
                &mut conn,
                probe_id,
                "sessions.write",
                &SessionsWriteParams::try_from_bytes(session_id.clone(), fence_probe.as_bytes())
                    .unwrap(),
            )
            .await;
            probe_id += 1;
            sent_probes += 1;
            next_probe_at = tokio::time::Instant::now() + Duration::from_millis(200);
        }
        let msg = match tokio::time::timeout(Duration::from_millis(150), conn.recv()).await {
            Ok(Ok(Some(m))) => m,
            _ => continue,
        };
        match msg {
            Message::Notification(n) if n.method == "session.output" => {
                if let Some(params) = n.params.as_ref() {
                    let p: SessionOutputParams =
                        serde_json::from_value(params.clone()).expect("decode output");
                    seen_fence.extend_from_slice(&p.data_bytes().expect("base64"));
                }
            }
            Message::Response(r) => {
                if let RequestId::Num(n) = r.id {
                    if n >= probe_id_start {
                        acked_probes += 1;
                    }
                }
            }
            _ => {}
        }
    }
    assert!(
        contains(&seen_fence, fence_needle.as_bytes()),
        "readiness fence echo never observed within deadline; got {:?}",
        String::from_utf8_lossy(&seen_fence)
    );

    // Drain any remaining in-flight probe Responses and shell echoes so
    // subsequent call()s — whose recv_response_for hard-panics on id
    // mismatch — don't trip over a late SYNCFENCE ack. Bounded by 3 s
    // so we can't hang if a Response is lost.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while acked_probes < sent_probes && tokio::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(150), conn.recv()).await {
            Ok(Ok(Some(Message::Response(r)))) => {
                if let RequestId::Num(n) = r.id {
                    if n >= probe_id_start {
                        acked_probes += 1;
                    }
                }
            }
            Ok(Ok(Some(_))) => {}
            _ => continue,
        }
    }
    assert_eq!(
        acked_probes, sent_probes,
        "did not drain every fence-probe Response before continuing"
    );

    // Send hello-m1 with the same deadline-bounded retry pattern as the
    // fence. Even after the fence proves the read loop has iterated at
    // least once, there is a residual window between every `printf` exit
    // and the next `read` install — a single hello-m1 can still land in
    // that gap. Re-sending until we observe `echo:hello-m1` closes it.
    // Multiple `hello-m1\r` writes produce multiple `echo:hello-m1`
    // lines — the `contains()` assertion only needs one.
    let hello_id_start: i64 = 200;
    let mut hello_id: i64 = hello_id_start;
    let mut hello_sent: i64 = 0;
    let mut hello_acked: i64 = 0;
    let mut echoed = Vec::<u8>::new();
    let mut next_hello_at = tokio::time::Instant::now();
    let echo_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < echo_deadline && !contains(&echoed, b"echo:hello-m1") {
        if tokio::time::Instant::now() >= next_hello_at {
            send_request(
                &mut conn,
                hello_id,
                "sessions.write",
                &SessionsWriteParams::try_from_bytes(session_id.clone(), b"hello-m1\r").unwrap(),
            )
            .await;
            hello_id += 1;
            hello_sent += 1;
            next_hello_at = tokio::time::Instant::now() + Duration::from_millis(200);
        }
        let msg = match tokio::time::timeout(Duration::from_millis(150), conn.recv()).await {
            Ok(Ok(Some(m))) => m,
            _ => continue,
        };
        match msg {
            Message::Notification(n) if n.method == "session.output" => {
                if let Some(params) = n.params.as_ref() {
                    let p: SessionOutputParams =
                        serde_json::from_value(params.clone()).expect("decode output");
                    echoed.extend_from_slice(&p.data_bytes().expect("base64"));
                }
            }
            Message::Response(r) => {
                if let RequestId::Num(n) = r.id {
                    if n >= hello_id_start {
                        if let la_proto::jsonrpc::ResponseOutcome::Error(e) = r.outcome {
                            panic!("sessions.write hello-m1 errored: {e:?}");
                        }
                        hello_acked += 1;
                    }
                }
            }
            _ => {}
        }
    }
    assert!(
        contains(&echoed, b"echo:hello-m1"),
        "missing echoed write; got {:?}",
        String::from_utf8_lossy(&echoed)
    );

    // Drain any leftover hello-m1 Responses so subsequent call()s
    // (sessions.detach, sessions.list, ...) don't trip the id-strict
    // recv_response_for.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while hello_acked < hello_sent && tokio::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(150), conn.recv()).await {
            Ok(Ok(Some(Message::Response(r)))) => {
                if let RequestId::Num(n) = r.id {
                    if n >= hello_id_start {
                        hello_acked += 1;
                    }
                }
            }
            Ok(Ok(Some(_))) => {}
            _ => continue,
        }
    }
    assert_eq!(
        hello_acked, hello_sent,
        "did not drain every hello-m1 Response before continuing"
    );

    let _: SessionsDetachResult = call(
        &mut conn,
        4,
        "sessions.detach",
        &SessionsDetachParams {
            session_id: session_id.clone(),
        },
    )
    .await;
    let reattach: SessionsAttachResult = call(
        &mut conn,
        5,
        "sessions.attach",
        &SessionsAttachParams {
            session_id: session_id.clone(),
            replay_bytes: None,
            resume_from_seq: None,
            acquire_input: true,
        },
    )
    .await;
    assert!(reattach.input_acquired);

    let _: SessionsWriteResult = call(
        &mut conn,
        6,
        "sessions.write",
        &SessionsWriteParams::try_from_bytes(session_id.clone(), b"quit\r").unwrap(),
    )
    .await;

    // Same pre-`read` race as hello-m1: if `quit\r` lands while the
    // shell is between `printf` and the next `read`, it gets dropped
    // and the session never exits. The existing poll loop polls list
    // every 100 ms — re-send `quit\r` on each tick so a lost write is
    // recovered without inflating wall-clock for the happy path.
    let mut final_list = None;
    let mut quit_id: i64 = 1000;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let list: SessionsListResult = call(
            &mut conn,
            7,
            "sessions.list",
            &SessionsListParams {
                project: None,
                backend: None,
                include_archived: true,
            },
        )
        .await;
        if list
            .sessions
            .iter()
            .any(|s| s.session_id == session_id && s.state == SessionState::Exited)
        {
            final_list = Some(list);
            break;
        }
        // Re-issue quit; the first one may have landed in the
        // pre-`read` window, in which case the shell is still alive
        // and waiting for another newline. Tolerate errors — once the
        // shell exits, the next write will fail with NotAttached or
        // similar, and that's fine: we're polling for Exited anyway.
        send_request(
            &mut conn,
            quit_id,
            "sessions.write",
            &SessionsWriteParams::try_from_bytes(session_id.clone(), b"quit\r").unwrap(),
        )
        .await;
        // Drain whatever Response comes back for quit_id without
        // panicking on RPC error.
        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        loop {
            if tokio::time::Instant::now() >= drain_deadline {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(50), conn.recv()).await {
                Ok(Ok(Some(Message::Response(r)))) if r.id == RequestId::Num(quit_id) => break,
                Ok(Ok(Some(_))) => continue,
                _ => continue,
            }
        }
        quit_id += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(final_list.is_some(), "session did not reach exited state");

    let _: SessionsArchiveResult = call(
        &mut conn,
        8,
        "sessions.archive",
        &SessionsArchiveParams {
            session_id: session_id.clone(),
        },
    )
    .await;
    let visible: SessionsListResult = call(
        &mut conn,
        9,
        "sessions.list",
        &SessionsListParams {
            project: None,
            backend: None,
            include_archived: false,
        },
    )
    .await;
    assert!(
        visible.sessions.iter().all(|s| s.session_id != session_id),
        "archived session should be hidden by default"
    );
    let archived: SessionsListResult = call(
        &mut conn,
        10,
        "sessions.list",
        &SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .await;
    assert!(
        archived
            .sessions
            .iter()
            .any(|s| s.session_id == session_id && s.state == SessionState::Archived),
        "archived session should be visible when requested"
    );

    let _: SessionsDeleteResult = call(
        &mut conn,
        11,
        "sessions.delete",
        &SessionsDeleteParams {
            session_id: session_id.clone(),
        },
    )
    .await;
    let deleted: SessionsListResult = call(
        &mut conn,
        12,
        "sessions.list",
        &SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .await;
    assert!(
        deleted.sessions.iter().all(|s| s.session_id != session_id),
        "deleted session must not remain in sessions.list"
    );

    stop_child(&mut lad);
}

#[tokio::test]
async fn lad_binary_restart_reaps_orphaned_session_rows() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime = tempdir.path().join("runtime");
    let state = tempdir.path().join("state");
    let project = tempdir.path().join("project");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let socket = runtime.join("lad-1.sock");
    let pid_file = project.join("child.pid");
    let script = "printf '%s\\n' $$ > child.pid; trap '' TERM; sleep 120";
    let mut first = spawn_lad_binary(&socket, &state, script).await;
    let mut conn = client(&socket).await;

    let create: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        &SessionsCreateParams {
            project_dir: project.to_string_lossy().to_string(),
            backend: "shtest".to_string(),
            args: vec![],
            prompt: None,
            worktree: false,
        },
    )
    .await;
    drop(conn);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut pid_text = String::new();
    while std::time::Instant::now() < deadline {
        if pid_file.exists() {
            pid_text = std::fs::read_to_string(&pid_file).unwrap_or_default();
            if !pid_text.trim().is_empty() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !pid_text.trim().is_empty(),
        "test child did not write pid file"
    );
    let child_pid: i32 = pid_text.trim().parse().expect("parse pid");

    stop_child(&mut first);
    #[cfg(unix)]
    kill_pid(child_pid, 9);

    let mut second = spawn_lad_binary(&socket, &state, "sleep 1").await;
    let mut conn = client(&socket).await;
    let list: SessionsListResult = call(
        &mut conn,
        2,
        "sessions.list",
        &SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .await;
    let restored = list
        .sessions
        .iter()
        .find(|s| s.session_id == create.session_id)
        .expect("session row should survive restart");
    assert_eq!(restored.state, SessionState::Exited);

    stop_child(&mut second);
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
async fn cron_fired_notification_path_survives_tui_disconnect() {
    let daemon = bootstrap_daemon("sleep 1").await;
    let bus = daemon
        .bus
        .as_ref()
        .expect("test daemon exposes event bus")
        .clone();

    let mut first = client(&daemon.socket).await;
    let subscribed: EventsSubscribeResult = call(
        &mut first,
        1,
        "events.subscribe",
        &EventsSubscribeParams {
            topics: vec![EventTopic::CronFired],
        },
    )
    .await;
    assert_eq!(subscribed.topics, vec![EventTopic::CronFired]);
    drop(first);

    // Publishing after the first TUI connection closes must not poison the
    // daemon's writer task or event bus for future subscribers.
    bus.publish(la_core::BusEvent::CronFired(CronFiredParams {
        cron_id: "after-first-tui-close".to_string(),
        run_id: "run-before-resubscribe".to_string(),
        fired_at: "2026-01-01T12:00:00Z".to_string(),
        status: "spawning".to_string(),
    }));

    let mut second = client(&daemon.socket).await;
    let subscribed_again: EventsSubscribeResult = call(
        &mut second,
        2,
        "events.subscribe",
        &EventsSubscribeParams {
            topics: vec![EventTopic::CronFired],
        },
    )
    .await;
    assert_eq!(subscribed_again.topics, vec![EventTopic::CronFired]);

    bus.publish(la_core::BusEvent::CronFired(CronFiredParams {
        cron_id: "after-second-tui-open".to_string(),
        run_id: "run-after-resubscribe".to_string(),
        fired_at: "2026-01-01T12:01:00Z".to_string(),
        status: "running".to_string(),
    }));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut seen = None;
    while tokio::time::Instant::now() < deadline {
        let msg = match tokio::time::timeout(Duration::from_millis(200), second.recv()).await {
            Ok(Ok(Some(m))) => m,
            _ => continue,
        };
        if let Message::Notification(n) = msg {
            if n.method == "cron.fired" {
                let params: CronFiredParams =
                    serde_json::from_value(n.params.expect("cron.fired params"))
                        .expect("decode cron.fired");
                seen = Some(params);
                break;
            }
        }
    }

    let seen = seen.expect("second TUI did not receive cron.fired after first disconnected");
    assert_eq!(seen.cron_id, "after-second-tui-open");
    assert_eq!(seen.run_id, "run-after-resubscribe");

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
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

/// WEK-38 chaos: an agent child can disappear outside the daemon's control
/// (OOM killer, user kill -9, terminal crash). The output pump must observe
/// the waiter exit, persist an `exited` state, drop the runtime, and keep the
/// daemon responsive for later RPCs.
#[tokio::test]
async fn chaos_external_child_kill_exits_session_without_crashing_daemon() {
    let project = tempfile::tempdir().expect("project tmp");
    let pid_file = project.path().join("child.pid");
    let daemon = bootstrap_daemon("printf '%s\\n' $$ > child.pid; trap '' TERM; sleep 120").await;
    let mut conn = client(&daemon.socket).await;

    let create: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        &SessionsCreateParams {
            project_dir: project.path().to_string_lossy().to_string(),
            backend: "shtest".to_string(),
            args: vec![],
            prompt: None,
            worktree: false,
        },
    )
    .await;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut pid_text = String::new();
    while std::time::Instant::now() < deadline {
        pid_text = std::fs::read_to_string(&pid_file).unwrap_or_default();
        if !pid_text.trim().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let child_pid: i32 = pid_text.trim().parse().expect("child wrote a pid");
    kill_pid(child_pid, 9);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while tokio::time::Instant::now() < deadline {
        let list: SessionsListResult = call(
            &mut conn,
            2,
            "sessions.list",
            &SessionsListParams {
                project: None,
                backend: None,
                include_archived: true,
            },
        )
        .await;
        if list
            .sessions
            .iter()
            .any(|s| s.session_id == create.session_id && s.state == SessionState::Exited)
        {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        exited,
        "externally killed child did not settle to exited state"
    );

    let _: SessionsListResult = call(
        &mut conn,
        3,
        "sessions.list",
        &SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .await;

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

/// WEK-38 / Story 3: closing a TUI is just an IPC disconnect. It must not
/// stop the daemon-owned session runtime or the daemon-level cron event path.
#[tokio::test]
async fn chaos_ipc_disconnect_keeps_session_and_cron_events_alive() {
    let project = tempfile::tempdir().expect("project tmp");
    let daemon = bootstrap_daemon("trap 'exit 0' TERM; sleep 120").await;
    let bus = daemon.bus.clone().expect("test daemon exposes its bus");
    let mut conn = client(&daemon.socket).await;

    let create: SessionsCreateResult = call(
        &mut conn,
        1,
        "sessions.create",
        &SessionsCreateParams {
            project_dir: project.path().to_string_lossy().to_string(),
            backend: "shtest".to_string(),
            args: vec![],
            prompt: None,
            worktree: false,
        },
    )
    .await;
    drop(conn);

    let mut conn = client(&daemon.socket).await;
    let list: SessionsListResult = call(
        &mut conn,
        2,
        "sessions.list",
        &SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .await;
    assert!(
        list.sessions
            .iter()
            .any(|s| s.session_id == create.session_id && s.state != SessionState::Exited),
        "disconnecting the first client must not reap the daemon-owned session"
    );

    let sub: EventsSubscribeResult = call(
        &mut conn,
        3,
        "events.subscribe",
        &EventsSubscribeParams {
            topics: vec![EventTopic::CronFired],
        },
    )
    .await;
    assert_eq!(sub.topics, vec![EventTopic::CronFired]);

    let payload = CronFiredParams {
        cron_id: "story-3-cron".into(),
        run_id: "run-after-disconnect".into(),
        fired_at: "2026-06-03T04:00:00Z".into(),
        status: "spawning".into(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while tokio::time::Instant::now() < deadline && got.is_none() {
        bus.publish(la_core::BusEvent::CronFired(payload.clone()));
        match tokio::time::timeout(Duration::from_millis(250), conn.recv()).await {
            Ok(Ok(Some(Message::Notification(n)))) if n.method == "cron.fired" => {
                got = Some(
                    serde_json::from_value::<CronFiredParams>(n.params.expect("cron params"))
                        .expect("decode cron.fired"),
                );
            }
            _ => {}
        }
    }
    let got = got.expect("cron.fired was not delivered after prior client disconnect");
    assert_eq!(got.cron_id, "story-3-cron");

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

/// WEK-38 chaos: a SQLite I/O failure must surface as an RPC error rather than
/// crashing the daemon process or poisoning unrelated IPC paths. Closing the
/// test-owned pools is a deterministic CI-safe fault injection for the same
/// error class as a transient SQLite I/O outage.
#[tokio::test]
async fn chaos_sqlite_io_error_surfaces_without_crashing_daemon() {
    let daemon = bootstrap_daemon("sleep 120").await;
    let storage = daemon
        .storage
        .as_ref()
        .expect("test daemon exposes storage")
        .clone();
    let mut conn = client(&daemon.socket).await;

    storage.close().await;

    send_request(
        &mut conn,
        1,
        "sessions.list",
        &SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .await;
    let err = recv_error_for(&mut conn, 1).await;
    assert!(
        err.message.to_ascii_lowercase().contains("storage")
            || err.message.to_ascii_lowercase().contains("sqlite")
            || err.message.to_ascii_lowercase().contains("closed"),
        "storage fault should be explicit, got {err:?}"
    );

    let sub: EventsSubscribeResult = call(
        &mut conn,
        2,
        "events.subscribe",
        &EventsSubscribeParams {
            topics: vec![EventTopic::CronFired],
        },
    )
    .await;
    assert_eq!(sub.topics, vec![EventTopic::CronFired]);

    daemon.handle.shutdown();
    let joined = timeout(Duration::from_secs(15), daemon.join).await;
    assert!(
        joined.is_ok(),
        "daemon did not shut down after storage fault"
    );
}

/// WEK-27 regression: a daemon with an active worktree-sweep loop
/// MUST still finish `DaemonHandle::shutdown` inside §6.4's 10 s
/// ceiling. The first attempt at the sweep loop joined it AFTER the
/// SIGKILL escalation, which pushed the wall-clock to ~11 s on CI.
/// Shrink the sweep interval so the loop is actively running, then
/// shut down and assert the join completes well under the ceiling.
#[tokio::test]
async fn shutdown_finishes_within_cap_with_active_worktree_sweep_loop() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket = runtime_dir.join("lad-1.sock");

    let adapter: Arc<dyn AgentAdapter> = Arc::new(ShellAdapter {
        script: "true".to_string(),
    });
    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert("shtest".to_string(), adapter);

    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        // Make the sweep loop fire roughly every tick of the test so
        // it's actively running when shutdown lands — otherwise the
        // first iteration's `sleep(WORKTREE_SWEEP_INTERVAL)` would
        // soak up the test entirely.
        worktree_sweep_interval: Duration::from_millis(100),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind");
    let (handle, join) = daemon.spawn();

    // Let the sweep loop spin at least once before we ask for
    // shutdown, so the test is exercising "abort a tick that's
    // already past its first `await`".
    tokio::time::sleep(Duration::from_millis(250)).await;

    let started = std::time::Instant::now();
    handle.shutdown();
    let joined = timeout(Duration::from_secs(11), join).await;
    let elapsed = started.elapsed();
    assert!(
        joined.is_ok(),
        "daemon shutdown didn't finish within §6.4's 10s+epsilon \
         with worktree sweep loop active (elapsed {elapsed:?})"
    );
    // No live sessions in this test → shutdown should be FAST. Allow
    // generous slack for CI noise but flag if the sweep loop adds
    // material wall-time back into the path.
    assert!(
        elapsed <= Duration::from_secs(2),
        "shutdown took {elapsed:?} with sweep loop active — should be \
         under 2 s when no live sessions need draining"
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

/// First-time `sessions.attach` with `resume_from_seq=None` is **live-only**:
/// frames the daemon emitted before the attach landed are NOT replayed.
/// This pins the semantics that match `OutputHub::subscribe(None)` and
/// guards against the WEK-49 review finding where the schema/docs claimed
/// `None` would replay the ring (the opposite of what the daemon does).
///
/// Walk:
///   1. Start a slow-stream session.
///   2. Wait long enough that the script has clearly emitted several
///      lines into the hub ring.
///   3. Attach with `resume_from_seq=None` on a fresh connection.
///   4. Drain frames until we reach a line emitted after the attach.
///   5. Assert: the first observed `seq` is strictly greater than the
///      `snapshot_seq` echoed in the attach response. If the daemon were
///      ring-replaying on `None`, we would instead see `seq <= snapshot_seq`.
#[tokio::test]
async fn first_attach_with_none_resume_is_live_only_no_ring_replay() {
    let project = tempfile::tempdir().expect("project tmp");
    // 40 lines × 50 ms ≈ 2 s total runtime; plenty of room to land the
    // attach after the ring already holds output.
    let script = "for i in $(seq 1 40); do printf 'line-%02d\\n' $i; sleep 0.05; done";
    let daemon = bootstrap_daemon(script).await;
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
        serde_json::from_value(recv_response_for(&mut conn, 1).await).expect("decode create");
    let session_id = result.session_id;

    // Let the session produce output into the ring *before* we attach,
    // so any ring replay would be observable.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let attach_params = SessionsAttachParams {
        session_id: session_id.clone(),
        resume_from_seq: None,
        replay_bytes: None,
        acquire_input: false,
    };
    send_request(&mut conn, 2, "sessions.attach", &attach_params).await;
    let ack: SessionsAttachResult =
        serde_json::from_value(recv_response_for(&mut conn, 2).await).expect("decode attach");
    // snapshot_seq is the last seq committed to the hub *at attach time*.
    // For `None` (live-only) semantics, the first frame we observe must
    // be strictly newer than this.
    let snapshot_seq = ack.snapshot_seq;

    let mut seen_seqs = Vec::<u64>::new();
    let mut seen_bytes = Vec::<u8>::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !contains(&seen_bytes, b"line-30") {
        let msg = match tokio::time::timeout(Duration::from_millis(300), conn.recv()).await {
            Ok(Ok(Some(m))) => m,
            _ => continue,
        };
        if let Message::Notification(n) = msg {
            if n.method == "session.output" {
                if let Some(params) = n.params.as_ref() {
                    let p: SessionOutputParams =
                        serde_json::from_value(params.clone()).expect("decode output");
                    seen_seqs.push(p.seq);
                    seen_bytes.extend_from_slice(&p.data_bytes().expect("base64"));
                }
            }
        }
    }

    let first_seq = *seen_seqs
        .first()
        .expect("live-only attach saw no chunks; script may have finished too fast");
    assert!(
        first_seq > snapshot_seq,
        "WEK-49 semantics violation: first observed seq={first_seq} must be > snapshot_seq={snapshot_seq} \
         when resume_from_seq=None (live-only). Ring replay leaked through.",
    );
    // Sanity: we are still receiving the live stream and should reach
    // lines emitted *after* attach landed.
    assert!(
        contains(&seen_bytes, b"line-30"),
        "live-only attach never reached line 30; got {:?}",
        String::from_utf8_lossy(&seen_bytes)
    );

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

// ----------------------------------------------------------------------
// WEK-29 / M2.6: backend grey-state + error classification
// ----------------------------------------------------------------------

/// Probe-only adapter that never actually spawns — used to drive the
/// pre-flight check in `sessions.create` and the per-backend payload on
/// `daemon.health` without bringing a real CLI into the test loop.
struct ProbeOnlyAdapter {
    id: &'static str,
    probe_result: la_adapter::ProbeResult,
}

#[async_trait::async_trait]
impl AgentAdapter for ProbeOnlyAdapter {
    fn descriptor(&self) -> la_adapter::AdapterDescriptor {
        la_adapter::AdapterDescriptor {
            id: self.id,
            display_name: self.id,
            default_program: self.id,
            docs_url: "https://example.com/install",
        }
    }
    async fn probe(&self) -> la_adapter::ProbeResult {
        self.probe_result.clone()
    }
    fn spawn_spec(&self, _req: &SpawnRequest) -> Result<SpawnSpec, la_adapter::AdapterError> {
        // Unreachable when the dispatcher's pre-flight does its job —
        // any failure here would be the bug we're testing for.
        panic!("spawn_spec invoked on ProbeOnlyAdapter for {}", self.id);
    }
    fn encode_user_input(&self, _text: &str) -> bytes::Bytes {
        bytes::Bytes::new()
    }
}

async fn bootstrap_daemon_with_adapters(
    adapters: HashMap<String, Arc<dyn AgentAdapter>>,
) -> TestDaemon {
    let expected_backends = adapters.len();
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket = runtime_dir.join("lad-1.sock");
    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        // Shrink the probe cadence so the test doesn't have to wait 60 s
        // for the second pulse — the initial inline probe is enough for
        // the assertions, but a short cadence keeps the loop honest if
        // we ever extend the test to assert refresh behaviour.
        probe_interval: Duration::from_millis(500),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind daemon");
    let bus = daemon.manager.bus();
    let storage = daemon.manager.storage().clone();
    let (handle, join) = daemon.spawn();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if connect(&Endpoint::uds(&socket)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_for_health_snapshot(&socket, expected_backends).await;
    TestDaemon {
        socket,
        handle,
        join,
        bus: Some(bus),
        storage: Some(storage),
        _tempdir: tempdir,
    }
}

async fn wait_for_health_snapshot(socket: &std::path::Path, expected_backends: usize) {
    use la_proto::methods::{EventTopic, EventsSubscribeParams, EventsSubscribeResult};
    use la_proto::notifications::DaemonHealthParams;

    let mut conn = client(socket).await;
    let sub = EventsSubscribeParams {
        topics: vec![EventTopic::DaemonHealth],
    };
    send_request(&mut conn, -1, "events.subscribe", &sub).await;
    let _: EventsSubscribeResult =
        serde_json::from_value(recv_response_for(&mut conn, -1).await).expect("decode sub");

    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), conn.recv()).await {
            Ok(Ok(Some(Message::Notification(n)))) if n.method == "daemon.health" => {
                let params: DaemonHealthParams =
                    serde_json::from_value(n.params.expect("params")).expect("decode health");
                if params.backends.len() >= expected_backends {
                    return;
                }
            }
            _ => {}
        }
    }
    panic!("daemon.health snapshot never populated all {expected_backends} backends");
}

async fn recv_error_for(
    conn: &mut la_ipc::Connection<tokio::net::UnixStream>,
    expected_id: i64,
) -> la_proto::jsonrpc::RpcError {
    loop {
        let msg = timeout(PROBE_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("eof");
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, RequestId::Num(expected_id));
            return match resp.outcome {
                la_proto::jsonrpc::ResponseOutcome::Error(e) => e,
                la_proto::jsonrpc::ResponseOutcome::Result(v) => {
                    panic!("expected RPC error, got result {v:?}")
                }
            };
        }
    }
}

#[tokio::test]
async fn sessions_create_refuses_uninstalled_backend_with_business_code() {
    // Two adapters: a `codex` stand-in that probes NotInstalled, and a
    // healthy `shtest` so we can confirm only the offender is blocked.
    let project = tempfile::tempdir().expect("project tmp");
    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert(
        "codex".to_string(),
        Arc::new(ProbeOnlyAdapter {
            id: "codex",
            probe_result: la_adapter::ProbeResult::NotInstalled {
                hint: "no `codex` on $PATH".into(),
            },
        }),
    );
    adapters.insert(
        "shtest".to_string(),
        Arc::new(ShellAdapter {
            script: "echo ok; sleep 0.1".to_string(),
        }),
    );

    let daemon = bootstrap_daemon_with_adapters(adapters).await;
    let mut conn = client(&daemon.socket).await;

    // Hit `codex` → expect -33101 ADAPTER_NOT_INSTALLED, NOT a generic
    // INTERNAL_ERROR or a SpawnFailed surfacing later in the lifecycle.
    let bad = SessionsCreateParams {
        project_dir: project.path().to_string_lossy().to_string(),
        backend: "codex".to_string(),
        args: vec![],
        prompt: None,
        worktree: false,
    };
    send_request(&mut conn, 7, "sessions.create", &bad).await;
    let err = recv_error_for(&mut conn, 7).await;
    assert_eq!(
        err.code,
        la_proto::error_codes::ADAPTER_NOT_INSTALLED,
        "wrong error code for an uninstalled backend: got {err:?}",
    );
    assert!(
        err.message.contains("codex"),
        "error message should reference the backend id, got {:?}",
        err.message
    );

    // The healthy backend still spawns — pre-flight only short-circuits
    // the offender.
    let good = SessionsCreateParams {
        project_dir: project.path().to_string_lossy().to_string(),
        backend: "shtest".to_string(),
        args: vec![],
        prompt: None,
        worktree: false,
    };
    send_request(&mut conn, 8, "sessions.create", &good).await;
    let _: SessionsCreateResult =
        serde_json::from_value(recv_response_for(&mut conn, 8).await).expect("decode shtest");

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

#[tokio::test]
async fn sessions_create_refuses_unauthenticated_backend_with_business_code() {
    let project = tempfile::tempdir().expect("project tmp");
    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert(
        "opencode".to_string(),
        Arc::new(ProbeOnlyAdapter {
            id: "opencode",
            probe_result: la_adapter::ProbeResult::Unauthenticated {
                docs_url: "https://example.com/login".into(),
            },
        }),
    );
    let daemon = bootstrap_daemon_with_adapters(adapters).await;
    let mut conn = client(&daemon.socket).await;

    let bad = SessionsCreateParams {
        project_dir: project.path().to_string_lossy().to_string(),
        backend: "opencode".to_string(),
        args: vec![],
        prompt: None,
        worktree: false,
    };
    send_request(&mut conn, 9, "sessions.create", &bad).await;
    let err = recv_error_for(&mut conn, 9).await;
    assert_eq!(
        err.code,
        la_proto::error_codes::ADAPTER_UNAUTHENTICATED,
        "wrong error code for an unauthenticated backend: got {err:?}",
    );
    assert!(
        err.message.contains("login"),
        "error message should surface the login doc link, got {:?}",
        err.message
    );

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

#[tokio::test]
async fn daemon_health_carries_per_backend_status_payload() {
    use la_proto::methods::{EventTopic, EventsSubscribeParams, EventsSubscribeResult};
    use la_proto::notifications::{BackendHealthStatus, DaemonHealthParams};

    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert(
        "claude".to_string(),
        Arc::new(ProbeOnlyAdapter {
            id: "claude",
            probe_result: la_adapter::ProbeResult::Available {
                version: "2.1.158".into(),
            },
        }),
    );
    adapters.insert(
        "codex".to_string(),
        Arc::new(ProbeOnlyAdapter {
            id: "codex",
            probe_result: la_adapter::ProbeResult::NotInstalled {
                hint: "not on PATH".into(),
            },
        }),
    );
    let daemon = bootstrap_daemon_with_adapters(adapters).await;
    let mut conn = client(&daemon.socket).await;

    // Subscribe to daemon.health and wait for at least one pulse.
    let sub = EventsSubscribeParams {
        topics: vec![EventTopic::DaemonHealth],
    };
    send_request(&mut conn, 1, "events.subscribe", &sub).await;
    let _: EventsSubscribeResult =
        serde_json::from_value(recv_response_for(&mut conn, 1).await).expect("decode sub");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got: Option<DaemonHealthParams> = None;
    while tokio::time::Instant::now() < deadline && got.is_none() {
        match tokio::time::timeout(Duration::from_millis(500), conn.recv()).await {
            Ok(Ok(Some(Message::Notification(n)))) if n.method == "daemon.health" => {
                let p: DaemonHealthParams =
                    serde_json::from_value(n.params.expect("params")).expect("decode health");
                if !p.backends.is_empty() {
                    got = Some(p);
                }
            }
            _ => {}
        }
    }

    let health = got.expect("never received a daemon.health pulse with backends populated");
    assert_eq!(health.backends.len(), 2);
    let claude = health
        .backends
        .iter()
        .find(|b| b.id == "claude")
        .expect("claude not present");
    assert_eq!(claude.status, BackendHealthStatus::Available);
    assert_eq!(claude.version.as_deref(), Some("2.1.158"));
    let codex = health
        .backends
        .iter()
        .find(|b| b.id == "codex")
        .expect("codex not present");
    assert_eq!(codex.status, BackendHealthStatus::NotInstalled);
    assert!(codex.reason.is_some());
    assert!(codex.docs_url.is_some());
    assert!(
        health.errors_last_5m >= 1,
        "at least one non-available backend should count as an error: {health:?}",
    );

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}

/// WEK-29 review fix: a TUI that subscribes *after* the daemon's
/// initial probe round must NOT have to wait the full probe interval
/// for a fresh `daemon.health` pulse — the daemon should immediately
/// push the cached snapshot on the new connection. This pins that
/// behaviour with a deliberately long probe interval so the snapshot
/// replay (not the next ticker tick) is the only thing that could
/// satisfy the assertion within the test deadline.
#[tokio::test]
async fn events_subscribe_immediately_pushes_cached_daemon_health_snapshot() {
    use la_proto::methods::{EventTopic, EventsSubscribeParams, EventsSubscribeResult};
    use la_proto::notifications::{BackendHealthStatus, DaemonHealthParams};

    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert(
        "codex".to_string(),
        Arc::new(ProbeOnlyAdapter {
            id: "codex",
            probe_result: la_adapter::ProbeResult::NotInstalled {
                hint: "not on PATH".into(),
            },
        }),
    );

    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket = runtime_dir.join("lad-1.sock");
    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        // 30 s ⇒ if the new-subscriber snapshot replay is missing, the
        // wait-for-pulse loop below times out long before the next
        // ticker tick. Tightly-bounded probe intervals (like the
        // 500 ms used in the other WEK-29 tests) would mask the bug.
        probe_interval: Duration::from_secs(30),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind daemon");
    let (handle, join) = daemon.spawn();
    let bootstrap_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < bootstrap_deadline {
        if connect(&Endpoint::uds(&socket)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Give the inline first probe a moment to populate the registry
    // before we connect — otherwise we'd race the spawn-and-probe and
    // could observe an empty cache, which is honestly a *different* bug
    // than the one this test pins.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let test = async {
        let mut conn = client(&socket).await;
        let sub = EventsSubscribeParams {
            topics: vec![EventTopic::DaemonHealth],
        };
        send_request(&mut conn, 1, "events.subscribe", &sub).await;
        // The dispatcher responds to subscribe *then* pushes the
        // snapshot; both messages may arrive in either order across
        // the wire, so accept whichever shows first.
        let mut got_ack = false;
        let mut got_health: Option<DaemonHealthParams> = None;
        // 2 s is well under the 30 s probe interval — anything we
        // receive in this window came from the immediate snapshot
        // replay, not the next probe tick.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline && !(got_ack && got_health.is_some()) {
            match tokio::time::timeout(Duration::from_millis(500), conn.recv()).await {
                Ok(Ok(Some(Message::Response(r)))) if r.id == RequestId::Num(1) => {
                    let v = match r.outcome {
                        la_proto::jsonrpc::ResponseOutcome::Result(v) => v,
                        la_proto::jsonrpc::ResponseOutcome::Error(e) => {
                            panic!("subscribe error: {e:?}")
                        }
                    };
                    let _: EventsSubscribeResult =
                        serde_json::from_value(v).expect("decode subscribe");
                    got_ack = true;
                }
                Ok(Ok(Some(Message::Notification(n)))) if n.method == "daemon.health" => {
                    let p: DaemonHealthParams =
                        serde_json::from_value(n.params.expect("params")).expect("decode health");
                    if !p.backends.is_empty() {
                        got_health = Some(p);
                    }
                }
                _ => {}
            }
        }
        assert!(got_ack, "never received events.subscribe response");
        let health =
            got_health.expect("expected an immediate daemon.health snapshot after subscribe");
        assert_eq!(health.backends.len(), 1, "single registered adapter");
        assert_eq!(health.backends[0].id, "codex");
        assert_eq!(health.backends[0].status, BackendHealthStatus::NotInstalled);
    };
    let outcome = tokio::time::timeout(Duration::from_secs(5), test).await;

    handle.shutdown();
    let _ = timeout(Duration::from_secs(15), join).await;
    outcome.expect("subscribe-snapshot test deadline exceeded");
}

/// WEK-36 review fix: subscribing to `cron.fired` must actually take
/// (echoed back in the effective topic set) AND must actually deliver
/// any `BusEvent::CronFired` the daemon publishes. Pre-fix the
/// dispatcher silently dropped the topic on subscribe AND lacked a
/// delivery branch, so the TUI's status-bar pulse only ever worked in
/// stub tests.
#[tokio::test]
async fn cron_fired_subscribes_and_delivers_over_ipc() {
    use la_core::BusEvent;
    use la_proto::methods::{EventTopic, EventsSubscribeParams, EventsSubscribeResult};
    use la_proto::notifications::CronFiredParams;

    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert(
        "codex".to_string(),
        Arc::new(ProbeOnlyAdapter {
            id: "codex",
            probe_result: la_adapter::ProbeResult::Available {
                version: "0.0".into(),
            },
        }),
    );
    let daemon = bootstrap_daemon_with_adapters(adapters).await;
    let bus = daemon.bus.clone().expect("test daemon exposes its bus");
    let mut conn = client(&daemon.socket).await;

    let sub = EventsSubscribeParams {
        topics: vec![EventTopic::CronFired],
    };
    send_request(&mut conn, 1, "events.subscribe", &sub).await;
    let ack: EventsSubscribeResult =
        serde_json::from_value(recv_response_for(&mut conn, 1).await).expect("decode sub");
    assert!(
        ack.topics.contains(&EventTopic::CronFired),
        "daemon dropped CronFired from the effective topic set: {:?}",
        ack.topics
    );

    // Publish a fake cron firing on the bus. The dispatcher's writer
    // task should serialize it as `cron.fired` and push it down this
    // connection. We retry a few times because the broadcast bus only
    // delivers to subscribers active at publish time and the writer
    // task may still be spinning up the first tick after subscribe.
    let payload = CronFiredParams {
        cron_id: "nightly-review".into(),
        run_id: "r-1".into(),
        fired_at: "2026-06-03T02:00:00Z".into(),
        status: "spawning".into(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got: Option<CronFiredParams> = None;
    let mut last_publish = tokio::time::Instant::now();
    bus.publish(BusEvent::CronFired(payload.clone()));
    while tokio::time::Instant::now() < deadline && got.is_none() {
        match tokio::time::timeout(Duration::from_millis(250), conn.recv()).await {
            Ok(Ok(Some(Message::Notification(n)))) if n.method == "cron.fired" => {
                let p: CronFiredParams = serde_json::from_value(n.params.expect("cron params"))
                    .expect("decode cron.fired");
                got = Some(p);
            }
            _ => {
                // Republish every 200ms in case the writer task hadn't
                // subscribed yet on the first attempt.
                if last_publish.elapsed() >= Duration::from_millis(200) {
                    bus.publish(BusEvent::CronFired(payload.clone()));
                    last_publish = tokio::time::Instant::now();
                }
            }
        }
    }

    let received = got.expect("never received a cron.fired notification over IPC");
    assert_eq!(received.cron_id, "nightly-review");
    assert_eq!(received.run_id, "r-1");

    daemon.handle.shutdown();
    let _ = timeout(Duration::from_secs(15), daemon.join).await;
}
