#![cfg(unix)]

mod common;

use common::{bootstrap_daemon, call, client, FakeBackend, RPC_TIMEOUT};
use la_proto::jsonrpc::Message;
use la_proto::methods::{EventTopic, EventsSubscribeParams, EventsSubscribeResult};
use la_proto::notifications::{
    BackendHealthStatus, DaemonHealth, DaemonHealthParams, NotificationMethod,
};
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_backend_is_grey_state_health_not_panic() {
    let daemon = bootstrap_daemon(vec![
        FakeBackend::available("claude", "Claude Code"),
        FakeBackend::not_installed("codex", "Codex CLI"),
        FakeBackend::available("opencode", "OpenCode"),
    ])
    .await;
    let mut conn = client(&daemon.socket).await;

    let subscribed: EventsSubscribeResult = call(
        &mut conn,
        1,
        "events.subscribe",
        EventsSubscribeParams {
            topics: vec![EventTopic::DaemonHealth],
        },
    )
    .await;
    assert_eq!(subscribed.topics, vec![EventTopic::DaemonHealth]);

    let health = read_health_with_backends(&mut conn).await;
    let codex = health
        .backends
        .iter()
        .find(|b| b.id == "codex")
        .expect("codex health entry");
    assert_eq!(codex.status, BackendHealthStatus::NotInstalled);
    assert!(
        codex
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("missing"),
        "missing install hint: {codex:?}"
    );
    for id in ["claude", "opencode"] {
        let backend = health
            .backends
            .iter()
            .find(|b| b.id == id)
            .unwrap_or_else(|| panic!("{id} health entry"));
        assert_eq!(
            backend.status,
            BackendHealthStatus::Available,
            "{id} should remain available while codex is grey-stated"
        );
    }
}

async fn read_health_with_backends(
    conn: &mut la_ipc::Connection<tokio::net::UnixStream>,
) -> DaemonHealthParams {
    let deadline = tokio::time::Instant::now() + RPC_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let msg = timeout(std::time::Duration::from_millis(250), conn.recv())
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        let Some(Message::Notification(n)) = msg else {
            continue;
        };
        if n.method == DaemonHealth::NAME {
            let params: DaemonHealthParams =
                serde_json::from_value(n.params.expect("health params")).expect("decode health");
            if params.backends.len() >= 3 {
                return params;
            }
        }
    }
    panic!("timed out waiting for daemon.health backends");
}
