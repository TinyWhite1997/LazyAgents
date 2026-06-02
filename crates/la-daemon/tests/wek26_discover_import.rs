//! WEK-26 / M2.3 — `adapters.discover` + `sessions.import` end-to-end.
//!
//! Covers the acceptance bar from the issue body:
//!
//! - Discover surfaces at least one session per adapter from a fixture
//!   on-disk store.
//! - Importing it lands a row in the daemon's `sessions` table whose
//!   `origin = 'import'` and `external_path` points at the fixture file
//!   (the daemon never copies / mutates that file).
//! - A second import call is idempotent — same `session_id` comes back,
//!   no duplicate row.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use la_adapter::codex::{CodexAdapter, SESSIONS_DIR_ENV};
use la_adapter::AgentAdapter;
use la_daemon::{Daemon, DaemonConfig, DaemonHandle, SocketDiscovery};
use la_ipc::transport::{connect, Endpoint};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request, RequestId};
use la_proto::methods::{
    AdaptersDiscoverParams, AdaptersDiscoverResult, SessionsImportParams, SessionsImportResult,
    SessionsListParams, SessionsListResult,
};
use tempfile::TempDir;
use tokio::time::timeout;

const RPC_TIMEOUT: Duration = Duration::from_secs(5);

struct TestDaemon {
    socket: PathBuf,
    handle: DaemonHandle,
    join: tokio::task::JoinHandle<()>,
    _tempdir: TempDir,
}

async fn bootstrap(adapters: HashMap<String, Arc<dyn AgentAdapter>>) -> TestDaemon {
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
        handle,
        join,
        _tempdir: tempdir,
    }
}

async fn client(socket: &std::path::Path) -> Connection<tokio::net::UnixStream> {
    let stream = connect(&Endpoint::uds(socket)).await.expect("connect");
    let mut conn = Connection::new(stream);
    let info = client_handshake(
        &mut conn,
        "wek26-test",
        "0.0.0",
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .expect("handshake");
    assert_eq!(info.protocol_version, la_proto::PROTOCOL_VERSION);
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
                la_proto::jsonrpc::ResponseOutcome::Result(v) => {
                    serde_json::from_value(v).expect("decode")
                }
                la_proto::jsonrpc::ResponseOutcome::Error(e) => panic!("rpc error: {e:?}"),
            };
        }
    }
}

/// Lay out a fixture `~/.codex/sessions` tree the codex adapter knows
/// how to walk: nested `YYYY/MM/DD/rollout-*.jsonl` with one
/// `session_meta` line apiece.
fn write_codex_fixture(root: &std::path::Path, project_a: &std::path::Path) -> PathBuf {
    let day = root.join("2026").join("06").join("03");
    std::fs::create_dir_all(&day).unwrap();
    let path = day.join("rollout-019e0000-0000-0000-0000-000000000aaa.jsonl");
    std::fs::write(
        &path,
        format!(
            "{{\"timestamp\":\"2026-06-03T08:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019e0000-0000-0000-0000-000000000aaa\",\"timestamp\":\"2026-06-03T08:00:00Z\",\"cwd\":\"{}\",\"originator\":\"codex_cli_rs\",\"cli_version\":\"0.135.0\"}}}}\n",
            project_a.display()
        ),
    )
    .unwrap();
    path
}

#[tokio::test]
async fn discover_then_import_lands_external_row_and_is_idempotent() {
    let project_a = tempfile::tempdir().expect("project tmp");
    let codex_root = tempfile::tempdir().expect("codex tmp");
    let fixture_path = write_codex_fixture(codex_root.path(), project_a.path());

    // Point the codex adapter's default discovery root at the fixture.
    // adapters.discover honours `source_path` per-call, but tests that
    // exercise the import path without an explicit override also pass
    // through this env, mirroring the adapter unit tests.
    std::env::set_var(SESSIONS_DIR_ENV, codex_root.path());

    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    adapters.insert("codex".to_string(), Arc::new(CodexAdapter::new()));
    let daemon = bootstrap(adapters).await;
    let mut conn = client(&daemon.socket).await;

    // 1) adapters.discover surfaces the fixture session.
    let discover: AdaptersDiscoverResult = call(
        &mut conn,
        10,
        "adapters.discover",
        AdaptersDiscoverParams {
            backend: Some("codex".into()),
            source_path: None,
            project_root: None,
        },
    )
    .await;
    assert_eq!(discover.discovered.len(), 1, "got {discover:?}");
    let ds = &discover.discovered[0];
    assert_eq!(ds.backend, "codex");
    assert_eq!(ds.external_id, "019e0000-0000-0000-0000-000000000aaa");
    assert_eq!(
        ds.external_path.as_deref(),
        Some(fixture_path.to_string_lossy().as_ref())
    );
    assert!(!ds.already_imported, "first discover must be fresh");
    assert_eq!(ds.created_at.as_deref(), Some("2026-06-03T08:00:00Z"));

    // 2) sessions.import promotes it to a native row.
    let imported: SessionsImportResult = call(
        &mut conn,
        20,
        "sessions.import",
        SessionsImportParams {
            backend: "codex".into(),
            source_path: None,
            external_ids: Some(vec!["019e0000-0000-0000-0000-000000000aaa".into()]),
        },
    )
    .await;
    assert_eq!(imported.imported.len(), 1);
    let row = &imported.imported[0];
    assert_eq!(row.external_id, "019e0000-0000-0000-0000-000000000aaa");
    assert_eq!(row.backend, "codex");
    assert!(!row.already_existed, "first import must create");
    assert_eq!(
        row.external_path.as_deref(),
        Some(fixture_path.to_string_lossy().as_ref())
    );
    let first_session_id = row.session_id.clone();

    // The fixture file must still be on disk, untouched.
    assert!(
        fixture_path.exists(),
        "import must NOT move or remove the source file"
    );

    // 3) sessions.list surfaces the row with origin='import'.
    let listed: SessionsListResult = call(
        &mut conn,
        30,
        "sessions.list",
        SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .await;
    let row = listed
        .sessions
        .iter()
        .find(|s| s.session_id == first_session_id)
        .expect("imported session present in list");
    assert_eq!(row.origin, "import");
    assert_eq!(row.backend, "codex");

    // 4) Re-importing must be idempotent — same session_id, no dup row.
    let again: SessionsImportResult = call(
        &mut conn,
        40,
        "sessions.import",
        SessionsImportParams {
            backend: "codex".into(),
            source_path: None,
            external_ids: Some(vec!["019e0000-0000-0000-0000-000000000aaa".into()]),
        },
    )
    .await;
    assert_eq!(again.imported.len(), 1);
    assert_eq!(again.imported[0].session_id, first_session_id);
    assert!(again.imported[0].already_existed);

    // And a fresh discover marks the row as already_imported.
    let discover2: AdaptersDiscoverResult = call(
        &mut conn,
        50,
        "adapters.discover",
        AdaptersDiscoverParams {
            backend: Some("codex".into()),
            source_path: None,
            project_root: None,
        },
    )
    .await;
    assert_eq!(discover2.discovered.len(), 1);
    assert!(
        discover2.discovered[0].already_imported,
        "second discover must mark imported rows"
    );

    std::env::remove_var(SESSIONS_DIR_ENV);
    daemon.handle.shutdown();
    let _ = daemon.join.await;
}
