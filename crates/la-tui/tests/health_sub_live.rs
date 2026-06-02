//! WEK-29 review fix coverage: the production `daemon.health` →
//! `AppMsg::BackendsUpdate` path actually fires when an upstream
//! daemon pushes a notification.
//!
//! Uses la-ipc directly to stub out a one-shot "fake daemon" — handshake,
//! ack `events.subscribe`, push one `daemon.health` notification — so we
//! don't pull la-daemon into la-tui dev-deps just to exercise the wire.
//! Asserts that `health_sub::spawn` decodes the notification and emits a
//! `HealthEvent::Backends` payload the runner can hand straight to App.

use std::time::Duration;

use la_ipc::transport::{Endpoint, Listener};
use la_ipc::{server_handshake, Connection};
use la_proto::jsonrpc::{Message, Response};
use la_proto::methods::{EventsSubscribeResult, ServerCapabilities};
use la_proto::notifications::{
    BackendHealth, BackendHealthStatus, DaemonHealth, DaemonHealthParams, NotificationMethod,
};
use la_tui::health_sub::{spawn as spawn_health_sub, HealthEvent};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_sub_forwards_daemon_health_notification_to_runner_channel() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-1.sock");

    // Stub "lad": accept one connection, run handshake, ack subscribe,
    // push one daemon.health notification, then idle until the client
    // disconnects (the la-tui health_sub thread exits when the runner
    // drops its Receiver).
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut conn = Connection::new(stream);
        let caps = ServerCapabilities {
            adapters: vec!["codex".into()],
            cron: false,
            worktree: false,
            diff: false,
            events: true,
        };
        server_handshake(
            &mut conn,
            "lad-stub",
            "0.0.0",
            &[la_proto::PROTOCOL_VERSION],
            caps,
        )
        .await
        .expect("handshake");

        // Drain one request — the events.subscribe from la-tui — and
        // ack it. Any subsequent recv blocks until the client closes
        // the connection, which is what we want.
        let msg = conn.recv().await.expect("recv").expect("eof");
        let sub_id = match msg {
            Message::Request(r) => {
                assert_eq!(r.method, "events.subscribe");
                r.id
            }
            other => panic!("expected events.subscribe request, got {other:?}"),
        };
        let resp = Response::success(
            sub_id,
            &EventsSubscribeResult {
                topics: vec![la_proto::methods::EventTopic::DaemonHealth],
            },
        )
        .expect("encode sub result");
        conn.send(&Message::Response(resp)).await.expect("send ack");

        // Push the snapshot the TUI is waiting for.
        let payload = DaemonHealthParams {
            queue_depth: 0,
            running: 0,
            errors_last_5m: 1,
            backends: vec![BackendHealth {
                id: "codex".into(),
                display_name: "Codex CLI".into(),
                status: BackendHealthStatus::NotInstalled,
                version: None,
                reason: Some("not on PATH".into()),
                docs_url: Some("https://example.com/install".into()),
                last_probed_at: "2026-06-02T00:00:00Z".into(),
            }],
        };
        let n = la_proto::jsonrpc::Notification::new(DaemonHealth::NAME, &payload)
            .expect("encode health");
        conn.send(&Message::Notification(n))
            .await
            .expect("send health");

        // Park until the client drops; recv returns Ok(None) on EOF.
        let _ = conn.recv().await;
    });

    // Now spawn the la-tui subscriber against our stub and assert it
    // hands the badges up the channel.
    let rx = spawn_health_sub(&socket);
    // The pump uses std::sync::mpsc, so block in a separate thread to
    // avoid stalling the tokio runtime — `recv_timeout` is sync.
    let recv = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
        .await
        .expect("join");
    let event = recv.expect("never received a HealthEvent from the pump");
    let HealthEvent::Backends(badges) = event;
    assert_eq!(badges.len(), 1);
    assert_eq!(badges[0].id, "codex");
    assert_eq!(badges[0].status, BackendHealthStatus::NotInstalled);
    assert_eq!(badges[0].reason.as_deref(), Some("not on PATH"));

    // Drop the receiver so the pump thread exits, then await the stub.
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    // RAII closes the tempdir + UDS path.
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}
