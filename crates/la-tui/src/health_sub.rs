//! Background `daemon.health` → `AppMsg::BackendsUpdate` pump.
//!
//! WEK-29 / M2.6 review fix: the production `la` binary needs to
//! actually consume the daemon's per-backend probe broadcast, not just
//! the unit/render tests. This module owns:
//!
//! 1. A tokio current-thread runtime hosted on a dedicated OS thread
//!    (the TUI's main event loop is synchronous crossterm I/O — it
//!    cannot await).
//! 2. A connection to the daemon UDS that runs the standard
//!    [`la_ipc::client_handshake`] and then issues `events.subscribe`
//!    for [`EventTopic::DaemonHealth`].
//! 3. A pump that decodes each pushed `daemon.health` notification and
//!    forwards a [`HealthEvent`] up the [`std::sync::mpsc`] channel that
//!    [`crate::runner`] drains between input polls.
//!
//! The pump intentionally lives outside [`crate::App`] so the App stays
//! synchronous and unit-testable; the runner is the integration point
//! that turns [`HealthEvent`]s into [`crate::AppMsg::BackendsUpdate`].

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use la_ipc::transport::{connect, Endpoint};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request};
use la_proto::methods::{EventTopic, EventsSubscribeParams};
use la_proto::notifications::{
    BackendHealth as WireBackendHealth, DaemonHealth, DaemonHealthParams, NotificationMethod,
};

use crate::BackendBadge;

/// Outbound event from the subscriber thread.
///
/// `Backends` is the only variant today, but keeping the wrapper enum
/// means future bus topics (cron, session.state) can land here without a
/// channel-type churn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthEvent {
    Backends(Vec<BackendBadge>),
}

/// Spawn the daemon-health subscriber on a dedicated OS thread.
///
/// Returns the receive end of a sync channel the runner drains on every
/// tick. The thread owns its tokio runtime and the daemon connection;
/// it exits when the channel's sender is dropped (the runner detects EOF
/// via `try_recv` returning `Disconnected`).
///
/// On connection failure (e.g. daemon not running) the thread logs the
/// error and returns — the receiver simply stays silent and the TUI
/// renders the empty-state placeholder. A future iteration could add
/// reconnect-with-backoff; for M2.6 the daemon is auto-spawned in
/// [`crate::bin::la::bootstrap_daemon`], so connection at startup is
/// the only realistic path.
pub fn spawn(socket: &Path) -> Receiver<HealthEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    let socket = socket.to_path_buf();
    std::thread::Builder::new()
        .name("la-health-sub".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::warn!(%err, "health-sub: failed to build tokio runtime");
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(err) = run(&socket, tx).await {
                    tracing::warn!(%err, "health-sub pump exited");
                }
            });
        })
        .expect("spawn la-health-sub thread");
    rx
}

async fn run(socket: &Path, tx: Sender<HealthEvent>) -> Result<(), String> {
    let endpoint = endpoint_for(socket);
    let stream = tokio::time::timeout(Duration::from_secs(2), connect(&endpoint))
        .await
        .map_err(|_| format!("timed out connecting to {}", socket.display()))?
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let mut conn = Connection::new(stream);
    let _info = client_handshake(
        &mut conn,
        "la-health-sub",
        env!("CARGO_PKG_VERSION"),
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .map_err(|e| format!("handshake: {e}"))?;

    // Issue events.subscribe(DaemonHealth). The daemon replies with the
    // standard response **and** also pushes the cached health snapshot
    // immediately on the same connection (post WEK-29 review fix in
    // dispatcher::handle_events_subscribe), so we don't need to wait
    // for the next probe tick to grey-state.
    let params = EventsSubscribeParams {
        topics: vec![EventTopic::DaemonHealth],
    };
    let req = Request::new(1i64, "events.subscribe", &params)
        .map_err(|e| format!("encode events.subscribe: {e}"))?;
    conn.send(&Message::Request(req))
        .await
        .map_err(|e| format!("send subscribe: {e}"))?;

    loop {
        let msg = match conn.recv().await {
            Ok(Some(m)) => m,
            Ok(None) => return Err("daemon closed connection".into()),
            Err(e) => return Err(format!("recv: {e}")),
        };
        let n = match msg {
            Message::Notification(n) => n,
            // The subscribe response itself is a Response we ignore; any
            // other unsolicited Request is unexpected but harmless.
            _ => continue,
        };
        if n.method != DaemonHealth::NAME {
            continue;
        }
        let Some(params) = n.params else { continue };
        let params: DaemonHealthParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "health-sub: decode daemon.health failed");
                continue;
            }
        };
        let badges = wire_to_badges(&params.backends);
        // `send` errors only when the receiver was dropped, i.e. the TUI
        // has shut down. Exit the pump quietly in that case — no
        // diagnostic noise on a clean quit.
        if tx.send(HealthEvent::Backends(badges)).is_err() {
            return Ok(());
        }
    }
}

fn wire_to_badges(wire: &[WireBackendHealth]) -> Vec<BackendBadge> {
    wire.iter().map(BackendBadge::from_wire).collect()
}

fn endpoint_for(socket: &Path) -> Endpoint {
    #[cfg(unix)]
    {
        Endpoint::uds(socket)
    }
    #[cfg(not(unix))]
    {
        let name = format!(
            r"\\.\pipe\lazyagents-{}",
            socket.file_stem().and_then(|s| s.to_str()).unwrap_or("lad")
        );
        Endpoint::named_pipe(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use la_proto::notifications::BackendHealthStatus;

    #[test]
    fn wire_to_badges_preserves_order_and_fields() {
        let wire = vec![
            WireBackendHealth {
                id: "claude".into(),
                display_name: "Claude Code".into(),
                status: BackendHealthStatus::Available,
                version: Some("2.1.158".into()),
                reason: None,
                docs_url: None,
                last_probed_at: "2026-06-02T00:00:00Z".into(),
            },
            WireBackendHealth {
                id: "codex".into(),
                display_name: "Codex CLI".into(),
                status: BackendHealthStatus::NotInstalled,
                version: None,
                reason: Some("not on PATH".into()),
                docs_url: Some("https://example.com/install".into()),
                last_probed_at: "2026-06-02T00:00:00Z".into(),
            },
        ];
        let badges = wire_to_badges(&wire);
        assert_eq!(badges.len(), 2);
        assert_eq!(badges[0].id, "claude");
        assert_eq!(badges[0].status, BackendHealthStatus::Available);
        assert_eq!(badges[1].id, "codex");
        assert_eq!(badges[1].status, BackendHealthStatus::NotInstalled);
        assert_eq!(badges[1].reason.as_deref(), Some("not on PATH"));
        assert_eq!(
            badges[1].docs_url.as_deref(),
            Some("https://example.com/install")
        );
    }
}
