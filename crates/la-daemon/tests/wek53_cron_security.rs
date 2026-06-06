//! WEK-53 end-to-end acceptance: `crons.*` IPC handlers must enforce the
//! `cron_security` state machine, and every write to the `crons` table
//! must funnel through the daemon's [`crate::cron_control::CronControl`]
//! serializer.
//!
//! Coverage:
//!
//! 1. `new_cron_upsert_lands_disabled_even_if_caller_asks_for_enabled`
//!    — a fresh `crons.upsert` never produces an enabled row in one
//!    step; the wire response reports `cron.enabled = false`.
//! 2. `enable_requires_token_round_trip` — first
//!    `crons.set_enabled { enabled: true, token: None }` returns a
//!    `requires_confirmation` with a token; second call echoing the
//!    token actually enables; stale token gets rejected with
//!    `CRON_CONFIRMATION_REQUIRED`.
//! 3. `sensitive_field_edit_force_disables_and_invalidates_pending_token`
//!    — an enabled cron whose sensitive field changes (`prompt`) gets
//!    auto-disabled by the daemon, and any token issued before the
//!    edit is rejected with `CRON_CONFIRMATION_REQUIRED`.
//! 4. `bypass_token_rejected` — `crons.set_enabled { enabled: true,
//!    token: Some("not-issued") }` errors with
//!    `CRON_CONFIRMATION_REQUIRED`; the cron stays disabled.
//! 5. `pending_token_invalidated_by_sensitive_upsert_on_disabled_cron`
//!    — regression for the WEK-53 review TOCTOU: a token issued
//!    against a disabled cron's content A is unredeemable once the
//!    cron's sensitive snapshot has been mutated to B, even though
//!    the cron stayed disabled and `requires_reconfirmation` never
//!    fired on the upsert side.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{AdapterDescriptor, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec};
use la_daemon::{Daemon, DaemonConfig, SchedulerConfig, SocketDiscovery};
use la_ipc::transport::{connect, Endpoint};
use la_ipc::{client_handshake, Connection};
use la_proto::error_codes;
use la_proto::jsonrpc::{Message, Request, RequestId};
use la_proto::methods::{
    CronsGetParams, CronsGetResult, CronsSetEnabledParams, CronsSetEnabledResult,
    CronsUpsertParams, CronsUpsertResult,
};
use tempfile::TempDir;
use tokio::time::timeout;

struct NoopAdapter;

#[async_trait]
impl AgentAdapter for NoopAdapter {
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
        // None of the WEK-53 tests fire crons; this is only here to
        // satisfy the `adapters` registry so `crons.upsert` accepts the
        // `backend = "shtest"` value.
        Ok(SpawnSpec {
            program: PathBuf::from("/bin/true"),
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
    adapters.insert("shtest".into(), Arc::new(NoopAdapter));

    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        scheduler: SchedulerConfig::default(),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind");
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
        handle,
        join,
        tempdir,
    }
}

async fn seed_project_row(state_dir: &std::path::Path) -> String {
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
            root_path: "/tmp".into(),
            display_name: "wek53-project".into(),
            vcs: None,
        })
        .await
        .expect("create project");
    storage.close().await;
    project_id
}

async fn client(socket: &std::path::Path) -> Connection<tokio::net::UnixStream> {
    let stream = connect(&Endpoint::uds(socket)).await.expect("connect");
    let mut conn = Connection::new(stream);
    let _ = client_handshake(&mut conn, "wek53", "0.0.0", &[la_proto::PROTOCOL_VERSION])
        .await
        .expect("handshake");
    conn
}

static REQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
fn next_id() -> i64 {
    REQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

async fn call<T, R>(conn: &mut Connection<tokio::net::UnixStream>, method: &str, params: &T) -> R
where
    T: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let req = Request {
        jsonrpc: la_proto::jsonrpc::Version,
        id: RequestId::Num(next_id()),
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
        if let Message::Response(r) = msg {
            if r.id == req.id {
                match r.outcome {
                    la_proto::jsonrpc::ResponseOutcome::Result(v) => {
                        return serde_json::from_value(v).expect("decode");
                    }
                    la_proto::jsonrpc::ResponseOutcome::Error(e) => {
                        panic!("RPC {method} errored: {e:?}");
                    }
                }
            }
        }
    }
}

async fn call_raw(
    conn: &mut Connection<tokio::net::UnixStream>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, la_proto::jsonrpc::RpcError> {
    let req = Request {
        jsonrpc: la_proto::jsonrpc::Version,
        id: RequestId::Num(next_id()),
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

fn upsert_params(project_id: &str, prompt: &str) -> CronsUpsertParams {
    CronsUpsertParams {
        id: None,
        name: "wek53".into(),
        project_id: project_id.into(),
        backend: "shtest".into(),
        spawn_args: serde_json::json!({}),
        prompt: prompt.into(),
        // Pick an expression that won't naturally fire during the test
        // window so we never spawn a real run.
        cron_expr: "0 0 1 1 *".into(),
        tz: "UTC".into(),
        catchup_mode: "coalesce".into(),
        max_concurrent_runs: 1,
        max_runs_per_day: 24,
        max_runtime_s: 60,
        cost_budget_usd_per_day: Some(1.5),
        failure_backoff: "expo(1m,2,1h)".into(),
        pause_on_consecutive_failures: 5,
    }
}

async fn shutdown(td: TestDaemon) {
    td.handle.shutdown();
    let _ = timeout(Duration::from_secs(5), td.join).await;
}

#[tokio::test]
async fn new_cron_upsert_lands_disabled_even_if_caller_asks_for_enabled() {
    let td = bootstrap().await;
    let project_id = seed_project_row(td.tempdir.path().join("state").as_path()).await;
    let mut conn = client(&td.socket).await;

    let up: CronsUpsertResult = call(
        &mut conn,
        "crons.upsert",
        &upsert_params(&project_id, "summarize logs"),
    )
    .await;

    // The wire result must show the cron landed disabled, and the daemon
    // did NOT pretend a sensitive-field reconfirmation was needed (this
    // is a fresh row, nothing to reconfirm against).
    assert!(!up.cron.enabled, "new cron must land disabled");
    assert!(!up.requires_reconfirmation);

    // Independently re-fetch to make sure storage agrees with the wire
    // response — guards against a "we said disabled but wrote enabled"
    // bug in CronControl.
    let got: CronsGetResult = call(
        &mut conn,
        "crons.get",
        &CronsGetParams {
            cron_id: up.cron.id.clone(),
        },
    )
    .await;
    assert!(!got.cron.enabled);

    shutdown(td).await;
}

#[tokio::test]
async fn enable_requires_token_round_trip() {
    let td = bootstrap().await;
    let project_id = seed_project_row(td.tempdir.path().join("state").as_path()).await;
    let mut conn = client(&td.socket).await;

    let up: CronsUpsertResult = call(
        &mut conn,
        "crons.upsert",
        &upsert_params(&project_id, "summarize logs"),
    )
    .await;
    let cron_id = up.cron.id;

    // First step: no token in, daemon issues one.
    let first: CronsSetEnabledResult = call(
        &mut conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: None,
        },
    )
    .await;
    assert!(!first.cron.enabled);
    let confirmation = first
        .requires_confirmation
        .as_ref()
        .expect("first call must return a confirmation");
    assert!(
        !confirmation.confirmation_token.is_empty(),
        "token must be non-empty"
    );
    assert_eq!(confirmation.prompt_preview, "summarize logs");
    assert_eq!(confirmation.daily_cost_budget.as_deref(), Some("$1.50/day"));

    let token = confirmation.confirmation_token.clone();

    // Second step: echo the token, cron flips to enabled.
    let second: CronsSetEnabledResult = call(
        &mut conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: Some(token.clone()),
        },
    )
    .await;
    assert!(second.cron.enabled);
    assert!(second.requires_confirmation.is_none());

    // Storage must agree.
    let got: CronsGetResult = call(
        &mut conn,
        "crons.get",
        &CronsGetParams {
            cron_id: cron_id.clone(),
        },
    )
    .await;
    assert!(got.cron.enabled);

    // Token is single-use: replaying it fails with
    // `CRON_CONFIRMATION_REQUIRED`.
    let err = call_raw(
        &mut conn,
        "crons.set_enabled",
        serde_json::to_value(CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: Some(token),
        })
        .unwrap(),
    )
    .await
    .expect_err("replayed token must be rejected");
    assert_eq!(err.code, error_codes::CRON_CONFIRMATION_REQUIRED);

    shutdown(td).await;
}

#[tokio::test]
async fn sensitive_field_edit_force_disables_and_invalidates_pending_token() {
    let td = bootstrap().await;
    let project_id = seed_project_row(td.tempdir.path().join("state").as_path()).await;
    let mut conn = client(&td.socket).await;

    // Set up: upsert + complete enable, so the cron is in the enabled
    // state we want to be force-disabled by the sensitive edit.
    let up: CronsUpsertResult = call(
        &mut conn,
        "crons.upsert",
        &upsert_params(&project_id, "summarize logs"),
    )
    .await;
    let cron_id = up.cron.id;

    let first: CronsSetEnabledResult = call(
        &mut conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: None,
        },
    )
    .await;
    let token = first.requires_confirmation.unwrap().confirmation_token;
    let _: CronsSetEnabledResult = call(
        &mut conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: Some(token),
        },
    )
    .await;

    // Issue a *new* pending token (first step only — do NOT consume).
    // The next sensitive-field upsert must invalidate this token.
    let pending: CronsSetEnabledResult = call(
        &mut conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            // Cron is already enabled; issuing another enable returns a
            // fresh token via the same state machine.
            // (The token machinery is consulted on every `enabled = true`
            // call so even an already-enabled cron rotates through it.)
            enabled: true,
            confirmation_token: None,
        },
    )
    .await;
    let pending_token = pending.requires_confirmation.unwrap().confirmation_token;

    // Sensitive edit: change `prompt`. With the existing row enabled,
    // CronControl must force-disable and invalidate `pending_token`.
    let mut next_params = upsert_params(&project_id, "summarize logs and email me");
    next_params.id = Some(cron_id.clone());
    let edited: CronsUpsertResult = call(&mut conn, "crons.upsert", &next_params).await;
    assert!(
        edited.requires_reconfirmation,
        "sensitive-field edit on enabled cron must report reconfirmation"
    );
    assert!(
        !edited.cron.enabled,
        "sensitive-field edit must force-disable the cron"
    );

    // The pending token from before the edit is now invalid.
    let err = call_raw(
        &mut conn,
        "crons.set_enabled",
        serde_json::to_value(CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: Some(pending_token),
        })
        .unwrap(),
    )
    .await
    .expect_err("token issued before sensitive edit must be invalidated");
    assert_eq!(err.code, error_codes::CRON_CONFIRMATION_REQUIRED);

    // Re-fetch to confirm storage agrees: cron is disabled with the new
    // prompt landed.
    let got: CronsGetResult = call(
        &mut conn,
        "crons.get",
        &CronsGetParams {
            cron_id: cron_id.clone(),
        },
    )
    .await;
    assert!(!got.cron.enabled);
    assert_eq!(got.cron.prompt, "summarize logs and email me");

    shutdown(td).await;
}

#[tokio::test]
async fn bypass_token_rejected() {
    let td = bootstrap().await;
    let project_id = seed_project_row(td.tempdir.path().join("state").as_path()).await;
    let mut conn = client(&td.socket).await;

    let up: CronsUpsertResult = call(
        &mut conn,
        "crons.upsert",
        &upsert_params(&project_id, "summarize logs"),
    )
    .await;
    let cron_id = up.cron.id;

    // Caller fabricates a token without ever asking the daemon. Reject.
    let err = call_raw(
        &mut conn,
        "crons.set_enabled",
        serde_json::to_value(CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: Some("definitely-not-a-real-token".into()),
        })
        .unwrap(),
    )
    .await
    .expect_err("bypass with fabricated token must be rejected");
    assert_eq!(err.code, error_codes::CRON_CONFIRMATION_REQUIRED);

    // Cron must still be disabled in storage.
    let got: CronsGetResult = call(
        &mut conn,
        "crons.get",
        &CronsGetParams {
            cron_id: cron_id.clone(),
        },
    )
    .await;
    assert!(!got.cron.enabled, "fabricated token must not enable");

    shutdown(td).await;
}

/// WEK-53 review regression: a confirmation token issued for a
/// **disabled** cron's content A must be unredeemable once a subsequent
/// upsert mutates that cron's sensitive snapshot to B. The original
/// implementation only invalidated tokens on the "enabled-cron sensitive
/// edit" path; a disabled cron could be re-targeted between issue and
/// confirm, letting the user unwittingly authorise content they were
/// never shown.
///
/// Repro shape (verbatim from the reviewer):
///   1. Upsert new disabled cron with prompt A.
///   2. crons.set_enabled { enabled: true, token: None } → token T.
///   3. crons.upsert same cron with sensitive field flipped to B
///      (cron is still disabled, so `requires_reconfirmation` itself
///      stays false — but T was issued against A).
///   4. crons.set_enabled { enabled: true, token: Some(T) } must error.
#[tokio::test]
async fn pending_token_invalidated_by_sensitive_upsert_on_disabled_cron() {
    let td = bootstrap().await;
    let project_id = seed_project_row(td.tempdir.path().join("state").as_path()).await;
    let mut conn = client(&td.socket).await;

    // Step 1: create disabled cron with prompt A.
    let up: CronsUpsertResult = call(
        &mut conn,
        "crons.upsert",
        &upsert_params(&project_id, "PROMPT-A"),
    )
    .await;
    let cron_id = up.cron.id;
    assert!(!up.cron.enabled);
    assert!(
        !up.requires_reconfirmation,
        "new cron has nothing to reconfirm against"
    );

    // Step 2: issue an enable token. Cron is still disabled.
    let first: CronsSetEnabledResult = call(
        &mut conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: None,
        },
    )
    .await;
    let confirmation = first
        .requires_confirmation
        .as_ref()
        .expect("first call must issue a token");
    assert_eq!(confirmation.prompt_preview, "PROMPT-A");
    let token = confirmation.confirmation_token.clone();
    assert!(!first.cron.enabled);

    // Step 3: sensitive-field upsert (prompt → B) on the still-disabled
    // cron. The wire response does NOT set `requires_reconfirmation`
    // here because the cron was not enabled, and that is exactly the
    // signal the original implementation latched onto — meaning it
    // would have kept the token alive. The fix is that the token now
    // carries a fingerprint of A's sensitive snapshot; switching to B
    // unbinds it at confirm time.
    let mut next_params = upsert_params(&project_id, "PROMPT-B");
    next_params.id = Some(cron_id.clone());
    let edited: CronsUpsertResult = call(&mut conn, "crons.upsert", &next_params).await;
    assert!(!edited.cron.enabled);
    assert!(
        !edited.requires_reconfirmation,
        "edit on disabled cron has no enabled state to flip — \
         this is exactly the path the original code missed"
    );

    // Step 4: replaying T against B's snapshot must be rejected. Without
    // the fix, this asserted `Ok(...)` with `cron.enabled = true` and
    // prompt = B, which is the attack.
    let err = call_raw(
        &mut conn,
        "crons.set_enabled",
        serde_json::to_value(CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: Some(token),
        })
        .unwrap(),
    )
    .await
    .expect_err("token issued for prompt A must not enable prompt B");
    assert_eq!(err.code, error_codes::CRON_CONFIRMATION_REQUIRED);

    // And the cron must still be disabled with the most recent (B)
    // content landed.
    let got: CronsGetResult = call(
        &mut conn,
        "crons.get",
        &CronsGetParams {
            cron_id: cron_id.clone(),
        },
    )
    .await;
    assert!(!got.cron.enabled);
    assert_eq!(got.cron.prompt, "PROMPT-B");

    // A fresh first-step call now reflects the B content the user
    // should be asked to authorise.
    let restart: CronsSetEnabledResult = call(
        &mut conn,
        "crons.set_enabled",
        &CronsSetEnabledParams {
            cron_id: cron_id.clone(),
            enabled: true,
            confirmation_token: None,
        },
    )
    .await;
    assert_eq!(
        restart
            .requires_confirmation
            .as_ref()
            .expect("restart issues a fresh token")
            .prompt_preview,
        "PROMPT-B",
        "restart of the two-step flow surfaces the current (B) preview"
    );

    shutdown(td).await;
}
