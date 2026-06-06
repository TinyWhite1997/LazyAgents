//! WEK-57 / M3.9 acceptance:
//!
//! 1. Daemon装配 scheduler: enabled crons load on bind, scheduler ticks
//!    keep firing after `TUI`-equivalent connections close.
//! 2. `crons.*` RPC surface (list/get/upsert/delete/set_enabled/run_now/
//!    dry_run) round-trips through the JSON-RPC dispatcher.
//! 3. `runs.*` RPC reads the audit + admitted rows the executor writes.
//! 4. Admission串行: with `global_max_concurrent_runs = 1` and 5
//!    concurrent `crons.run_now` calls, the executor admits at most one
//!    row in `running`-state; the other four come back as audit refusals
//!    with `error_kind = quota_global_max_concurrent_runs`.
//! 5. Graceful shutdown: buffered fires drain into `runs` before
//!    `Daemon::accept_loop` returns; no admission writes are lost.

// Talks to the daemon over the cross-platform IPC harness from
// la_ipc::transport (UDS on Unix, Named Pipe on Windows). Tests that
// actually spawn a `sh` script (cron run_now, graceful drain) are gated
// per-fn with `#[cfg(unix)]`; the upsert/list/error paths run tri-OS.

mod support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{AdapterDescriptor, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec};
use la_daemon::{Daemon, DaemonConfig, SchedulerConfig, SocketDiscovery};
use la_ipc::transport::{connect, endpoint_for, StreamPair};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request, RequestId};
use la_proto::methods::{
    CronsDryRunParams, CronsDryRunResult, CronsListParams, CronsListResult, CronsRunNowParams,
    CronsRunNowResult, CronsSetEnabledParams, CronsSetEnabledResult, CronsUpsertParams,
    CronsUpsertResult, RunsListParams, RunsListResult,
};
use la_scheduler::GlobalQuota;
use tempfile::TempDir;
use tokio::time::timeout;

/// Echo-and-exit adapter so spawned sessions terminate quickly enough for
/// the executor's session-state watcher to flip the run to `completed`
/// inside the test window. We use `sleep 0` because `printf` + immediate
/// EOF can race the session pump's first read; one shell tick gives the
/// PTY pump a chance to drain before exit.
struct EchoAdapter;

#[async_trait]
impl AgentAdapter for EchoAdapter {
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
        // Build a tiny script that exits quickly. Using `sh -c "true"` keeps
        // the cron run lifecycle fully contained inside the test.
        #[cfg(unix)]
        {
            let script_dir = std::env::temp_dir().join("lazyagents-wek57-scripts");
            std::fs::create_dir_all(&script_dir).map_err(la_adapter::AdapterError::SpawnFailed)?;
            let script_path = script_dir.join(format!("{}.sh", la_storage::new_id()));
            std::fs::write(&script_path, "#!/bin/sh\nexit 0\n")
                .map_err(la_adapter::AdapterError::SpawnFailed)?;
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&script_path)
                .map_err(la_adapter::AdapterError::SpawnFailed)?
                .permissions();
            perm.set_mode(0o700);
            std::fs::set_permissions(&script_path, perm)
                .map_err(la_adapter::AdapterError::SpawnFailed)?;
            Ok(SpawnSpec {
                program: script_path,
                args: vec![],
                env: req.env.clone(),
                cwd: req.cwd.clone(),
                pty: req.pty,
                stdin_mode: req.stdin_mode,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = req;
            // Only the cron `run_now` tests (Unix-only per-fn gated) exercise
            // spawn — never called on Windows.
            unreachable!("EchoAdapter::spawn_spec is only exercised by Unix-only tests")
        }
    }

    fn encode_user_input(&self, text: &str) -> Bytes {
        Bytes::copy_from_slice(text.as_bytes())
    }
}

/// Slow adapter that stays running until the test signals it. We use
/// `sleep N` so the run holds the global-running slot long enough for
/// the concurrent attempts to race.
struct SleepAdapter {
    seconds: u32,
}

#[async_trait]
impl AgentAdapter for SleepAdapter {
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
        #[cfg(unix)]
        {
            let secs = self.seconds;
            let script_dir = std::env::temp_dir().join("lazyagents-wek57-sleep-scripts");
            std::fs::create_dir_all(&script_dir).map_err(la_adapter::AdapterError::SpawnFailed)?;
            let script_path = script_dir.join(format!("{}.sh", la_storage::new_id()));
            std::fs::write(&script_path, format!("#!/bin/sh\nsleep {secs}\nexit 0\n"))
                .map_err(la_adapter::AdapterError::SpawnFailed)?;
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&script_path)
                .map_err(la_adapter::AdapterError::SpawnFailed)?
                .permissions();
            perm.set_mode(0o700);
            std::fs::set_permissions(&script_path, perm)
                .map_err(la_adapter::AdapterError::SpawnFailed)?;
            Ok(SpawnSpec {
                program: script_path,
                args: vec![],
                env: req.env.clone(),
                cwd: req.cwd.clone(),
                pty: req.pty,
                stdin_mode: req.stdin_mode,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (req, self.seconds);
            unreachable!("SleepAdapter::spawn_spec is only exercised by Unix-only tests")
        }
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

async fn bootstrap(adapter: Arc<dyn AgentAdapter>, scheduler_cfg: SchedulerConfig) -> TestDaemon {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket = support::unique_socket_path(&runtime_dir);
    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert("shtest".into(), adapter);

    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        scheduler: scheduler_cfg,
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind");
    let (handle, join) = daemon.spawn();

    // Wait briefly for the accept loop.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if connect(&endpoint_for(&socket)).await.is_ok() {
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

async fn client(socket: &std::path::Path) -> Connection<StreamPair> {
    let stream = connect(&endpoint_for(socket)).await.expect("connect");
    let mut conn = Connection::new(stream);
    let _ = client_handshake(&mut conn, "wek57", "0.0.0", &[la_proto::PROTOCOL_VERSION])
        .await
        .expect("handshake");
    conn
}

async fn call<T, R>(conn: &mut Connection<StreamPair>, method: &str, params: &T) -> R
where
    T: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let req = Request {
        jsonrpc: la_proto::jsonrpc::Version,
        id: RequestId::Num(rand_id()),
        method: method.to_string(),
        params: Some(serde_json::to_value(params).unwrap()),
    };
    conn.send(&Message::Request(req.clone()))
        .await
        .expect("send");
    loop {
        let msg = timeout(Duration::from_secs(5), conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("recv eof");
        match msg {
            Message::Response(r) if r.id == req.id => match r.outcome {
                la_proto::jsonrpc::ResponseOutcome::Result(v) => {
                    return serde_json::from_value(v).expect("decode result")
                }
                la_proto::jsonrpc::ResponseOutcome::Error(e) => {
                    panic!("RPC {method} errored: {e:?}");
                }
            },
            _ => continue,
        }
    }
}

async fn call_raw(
    conn: &mut Connection<StreamPair>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, la_proto::jsonrpc::RpcError> {
    let req = Request {
        jsonrpc: la_proto::jsonrpc::Version,
        id: RequestId::Num(rand_id()),
        method: method.to_string(),
        params: Some(params),
    };
    conn.send(&Message::Request(req.clone()))
        .await
        .expect("send");
    loop {
        let msg = timeout(Duration::from_secs(5), conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("recv eof");
        if let Message::Response(r) = msg {
            if r.id == req.id {
                return match r.outcome {
                    la_proto::jsonrpc::ResponseOutcome::Result(v) => Ok(v),
                    la_proto::jsonrpc::ResponseOutcome::Error(e) => Err(e),
                };
            }
        }
    }
}

static REQ_COUNTER: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
fn rand_id() -> i64 {
    REQ_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// WEK-53: enable a cron through the two-step confirmation flow. First
/// call returns a token; second call echoes it to actually flip
/// `enabled = true`. Most tests in this file do not exercise the token
/// state machine itself — they just need an enabled cron — so this
/// helper hides the round trip.
async fn enable_cron(
    conn: &mut Connection<StreamPair>,
    cron_id: &str,
) -> CronsSetEnabledResult {
    let first: CronsSetEnabledResult = call(
        conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.to_string(),
            enabled: true,
            confirmation_token: None,
        },
    )
    .await;
    let token = first
        .requires_confirmation
        .as_ref()
        .map(|c| c.confirmation_token.clone())
        .expect("first set_enabled should return a confirmation token");
    assert!(!first.cron.enabled, "first call must not enable");
    call(
        conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.to_string(),
            enabled: true,
            confirmation_token: Some(token),
        },
    )
    .await
}

/// Insert a project + cron straight through storage so the test can drive
/// the daemon's cron RPC without first walking sessions.create.
async fn seed_project_row(state_dir: &std::path::Path, root: &str) -> String {
    use la_storage::{BackendUpsert, NewProject, Storage, StorageConfig};
    let cfg = StorageConfig::new(state_dir.join("lad.sqlite"), state_dir.to_path_buf());
    let storage = Storage::open(cfg).await.expect("storage reopen");
    let project_id = la_storage::new_id();
    let _ = storage
        .backends()
        .upsert(BackendUpsert {
            id: "shtest",
            display_name: "Shell Test Backend",
            version: None,
            available: true,
        })
        .await;
    storage
        .projects()
        .create(NewProject {
            id: project_id.clone(),
            root_path: root.to_string(),
            display_name: "wek57-project".into(),
            vcs: None,
        })
        .await
        .expect("create project");
    storage.close().await;
    project_id
}

#[tokio::test]
async fn crons_upsert_list_dry_run_round_trip() {
    let td = bootstrap(Arc::new(EchoAdapter), SchedulerConfig::default()).await;

    // Seed a real project row so cron upsert satisfies the FK constraint.
    let project_id = seed_project_row(td._tempdir.path().join("state").as_path(), "/tmp").await;

    let mut conn = client(&td.socket).await;

    // dry_run is pure — exercise it without persisting.
    let dr: CronsDryRunResult = call(
        &mut conn,
        "crons.dry_run",
        &CronsDryRunParams {
            cron_expr: "*/5 * * * *".into(),
            tz: "UTC".into(),
            count: 3,
        },
    )
    .await;
    assert_eq!(dr.fires.len(), 3);

    // Upsert a cron, list it back.
    let upserted: CronsUpsertResult = call(
        &mut conn,
        "crons.upsert",
        &CronsUpsertParams {
            id: None,
            name: "nightly".into(),
            project_id: project_id.clone(),
            backend: "shtest".into(),
            spawn_args: serde_json::json!({}),
            prompt: "noop".into(),
            cron_expr: "*/10 * * * *".into(),
            tz: "UTC".into(),
            catchup_mode: "coalesce".into(),
            max_concurrent_runs: 1,
            max_runs_per_day: 24,
            max_runtime_s: 60,
            cost_budget_usd_per_day: None,
            failure_backoff: "expo(1m,2,1h)".into(),
            pause_on_consecutive_failures: 5,
        },
    )
    .await;
    assert_eq!(upserted.cron.name, "nightly");
    // Upsert starts as disabled until set_enabled.
    assert!(!upserted.cron.enabled);

    let enabled = enable_cron(&mut conn, &upserted.cron.id).await;
    assert!(enabled.cron.enabled);

    let listed: CronsListResult = call(
        &mut conn,
        "crons.list",
        &CronsListParams {
            project_id: Some(project_id),
            include_disabled: true,
        },
    )
    .await;
    assert!(listed.crons.iter().any(|c| c.id == upserted.cron.id));

    td.handle.shutdown();
    let _ = timeout(Duration::from_secs(5), td.join).await;
}

// Unix-only: actually spawns the sh-script EchoAdapter/SleepAdapter
// through the daemon's executor + PTY layer. Windows replacement would
// need a Win32-portable spawn fixture; tracked separately.
#[cfg(unix)]
#[tokio::test]
async fn crons_run_now_admits_through_admission_lock_with_global_cap_one() {
    // Global cap = 1 so even concurrent run_now calls cannot both spawn.
    let scheduler_cfg = SchedulerConfig {
        global: GlobalQuota {
            global_max_concurrent_runs: 1,
            cpu_load_throttle: None,
        },
        ..SchedulerConfig::default()
    };
    let td = bootstrap(Arc::new(SleepAdapter { seconds: 3 }), scheduler_cfg).await;
    let state_dir = td._tempdir.path().join("state");
    let project_id = seed_project_row(state_dir.as_path(), "/tmp").await;

    // Create 5 enabled crons.
    let mut conn = client(&td.socket).await;
    let mut cron_ids = Vec::new();
    for i in 0..5 {
        let up: CronsUpsertResult = call(
            &mut conn,
            "crons.upsert",
            &CronsUpsertParams {
                id: None,
                name: format!("cron-{i}"),
                project_id: project_id.clone(),
                backend: "shtest".into(),
                spawn_args: serde_json::json!({}),
                prompt: "noop".into(),
                cron_expr: "0 0 1 1 *".into(), // never naturally fires during the test
                tz: "UTC".into(),
                catchup_mode: "coalesce".into(),
                max_concurrent_runs: 1,
                max_runs_per_day: 100,
                max_runtime_s: 60,
                cost_budget_usd_per_day: None,
                failure_backoff: "expo(1m,2,1h)".into(),
                pause_on_consecutive_failures: 5,
            },
        )
        .await;
        let _ = enable_cron(&mut conn, &up.cron.id).await;
        cron_ids.push(up.cron.id);
    }

    // Concurrent run_now over the same TCP socket would serialise; fan out
    // each call onto its own connection so the daemon executor sees five
    // requests racing in real time.
    let mut handles = Vec::new();
    for cron_id in cron_ids.clone() {
        let sock = td.socket.clone();
        handles.push(tokio::spawn(async move {
            let mut c = client(&sock).await;
            let r: CronsRunNowResult =
                call(&mut c, "crons.run_now", &CronsRunNowParams { cron_id }).await;
            r
        }));
    }
    let mut admitted = 0;
    let mut refused = 0;
    for h in handles {
        let r = h.await.expect("join");
        if r.admitted {
            admitted += 1;
        } else {
            refused += 1;
            // Refusals must surface the global cap as their reason tag.
            let reason = r.refused.unwrap_or_default();
            assert!(
                reason.contains("global_max_concurrent_runs")
                    || reason.contains("max_concurrent_runs"),
                "unexpected refusal reason: {reason}"
            );
        }
    }
    assert_eq!(admitted, 1, "exactly one fire passes the global=1 gate");
    assert_eq!(refused, 4, "the other four become audit refusals");

    // runs.list must show 1 admitted + 4 cancelled audit rows.
    let mut conn2 = client(&td.socket).await;
    let listed: RunsListResult = call(
        &mut conn2,
        "runs.list",
        &RunsListParams {
            cron_id: None,
            since: None,
            limit: 50,
        },
    )
    .await;
    let admitted_rows = listed
        .runs
        .iter()
        .filter(|r| matches!(r.status.as_str(), "spawning" | "running" | "completed"))
        .count();
    let refused_rows = listed
        .runs
        .iter()
        .filter(|r| {
            r.error_kind
                .as_deref()
                .is_some_and(|k| k.starts_with("quota_"))
        })
        .count();
    assert!(
        admitted_rows >= 1,
        "at least one admitted row, got {admitted_rows}"
    );
    assert_eq!(refused_rows, 4, "four audit rows for the refused fires");

    // Strongest invariant from the issue body: at no point are there more
    // than `global_max_concurrent_runs` rows in {spawning, running}.
    let live_rows = listed
        .runs
        .iter()
        .filter(|r| matches!(r.status.as_str(), "spawning" | "running"))
        .count();
    assert!(
        live_rows <= 1,
        "global_max_concurrent_runs=1 must not be breached, got {live_rows}"
    );

    td.handle.shutdown();
    let _ = timeout(Duration::from_secs(10), td.join).await;
}

#[tokio::test]
async fn unknown_backend_upsert_returns_adapter_not_installed() {
    let td = bootstrap(Arc::new(EchoAdapter), SchedulerConfig::default()).await;
    let project_id = seed_project_row(td._tempdir.path().join("state").as_path(), "/tmp").await;
    let mut conn = client(&td.socket).await;
    let err = call_raw(
        &mut conn,
        "crons.upsert",
        serde_json::json!({
            "name": "bad",
            "project_id": project_id,
            "backend": "ghost",
            "prompt": "x",
            "cron_expr": "0 0 * * *",
        }),
    )
    .await
    .expect_err("ghost backend rejected");
    assert_eq!(err.code, la_proto::error_codes::ADAPTER_NOT_INSTALLED);
    td.handle.shutdown();
    let _ = timeout(Duration::from_secs(5), td.join).await;
}

#[tokio::test]
async fn invalid_cron_expr_returns_cron_invalid_expr() {
    let td = bootstrap(Arc::new(EchoAdapter), SchedulerConfig::default()).await;
    let project_id = seed_project_row(td._tempdir.path().join("state").as_path(), "/tmp").await;
    let mut conn = client(&td.socket).await;
    let err = call_raw(
        &mut conn,
        "crons.upsert",
        serde_json::json!({
            "name": "bad-expr",
            "project_id": project_id,
            "backend": "shtest",
            "prompt": "x",
            "cron_expr": "not a cron",
        }),
    )
    .await
    .expect_err("invalid expr");
    assert_eq!(err.code, la_proto::error_codes::CRON_INVALID_EXPR);
    td.handle.shutdown();
    let _ = timeout(Duration::from_secs(5), td.join).await;
}

// Unix-only: relies on the sh-script EchoAdapter actually spawning a
// PTY child for the cron run. Windows replacement deferred.
#[cfg(unix)]
#[tokio::test]
async fn graceful_shutdown_drains_pending_fires_into_runs_table() {
    // Boot a daemon, seed a cron, fire run_now → confirm `runs` row
    // exists, then shutdown and verify the row survives.
    //
    // `cpu_load_throttle: None` so a noisy CI/agent host cannot defer the
    // single `run_now` admission via the loadavg gate and turn this into
    // a flake (we only care that an admitted run survives shutdown, not
    // how the loadavg gate behaves).
    let scheduler_cfg = SchedulerConfig {
        global: GlobalQuota {
            cpu_load_throttle: None,
            ..GlobalQuota::default()
        },
        ..SchedulerConfig::default()
    };
    let td = bootstrap(Arc::new(EchoAdapter), scheduler_cfg).await;
    let state_dir = td._tempdir.path().join("state");
    let project_id = seed_project_row(state_dir.as_path(), "/tmp").await;
    let mut conn = client(&td.socket).await;

    let up: CronsUpsertResult = call(
        &mut conn,
        "crons.upsert",
        &CronsUpsertParams {
            id: None,
            name: "drain".into(),
            project_id: project_id.clone(),
            backend: "shtest".into(),
            spawn_args: serde_json::json!({}),
            prompt: "noop".into(),
            cron_expr: "0 0 1 1 *".into(),
            tz: "UTC".into(),
            catchup_mode: "coalesce".into(),
            max_concurrent_runs: 1,
            max_runs_per_day: 24,
            max_runtime_s: 60,
            cost_budget_usd_per_day: None,
            failure_backoff: "expo(1m,2,1h)".into(),
            pause_on_consecutive_failures: 5,
        },
    )
    .await;
    let _ = enable_cron(&mut conn, &up.cron.id).await;
    let r: CronsRunNowResult = call(
        &mut conn,
        "crons.run_now",
        &CronsRunNowParams {
            cron_id: up.cron.id.clone(),
        },
    )
    .await;
    assert!(r.admitted);

    // Issue shutdown; this triggers the §6.4 sequence including the
    // executor drain.
    td.handle.shutdown();
    let _ = timeout(Duration::from_secs(10), td.join).await;

    // Re-open storage and confirm the admitted run survived.
    use la_storage::{Storage, StorageConfig};
    let cfg = StorageConfig::new(state_dir.join("lad.sqlite"), state_dir.clone());
    let storage = Storage::open(cfg).await.expect("reopen");
    let runs = storage
        .runs()
        .list(la_storage::RunsListFilter {
            cron_id: Some(&up.cron.id),
            since: None,
            limit: 10,
        })
        .await
        .expect("list runs");
    assert!(!runs.is_empty(), "admitted run should survive shutdown");
    storage.close().await;
}
