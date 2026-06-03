//! Background daemon-notification pump.
//!
//! Generalises the M2.6 `health_sub` module: subscribes to **both**
//! [`EventTopic::DaemonHealth`] (per-backend probe state +
//! running/errors counters) and [`EventTopic::CronFired`] (cron trigger
//! pulses) on a single connection, then reconnects with backoff if the
//! daemon goes away — that is the load-bearing piece of WEK-36 / M3.5's
//! "daemon restart auto-recover" acceptance.
//!
//! The pump owns:
//!
//! 1. A tokio current-thread runtime hosted on a dedicated OS thread
//!    (the TUI's main event loop is synchronous crossterm I/O — it
//!    cannot await).
//! 2. A connection to the daemon UDS (or Windows Named Pipe) that runs
//!    the standard [`la_ipc::client_handshake`] and then issues
//!    `events.subscribe` for both topics.
//! 3. A pump that decodes each pushed notification and forwards a
//!    [`NotifEvent`] up the [`std::sync::mpsc`] channel that the
//!    runner drains between input polls.
//! 4. A reconnect loop: any error inside the per-connection driver
//!    surfaces a `NotifEvent::DaemonOffline` toast and re-establishes
//!    the connection after capped exponential backoff.
//!
//! The pump intentionally lives outside [`crate::App`] so the App stays
//! synchronous and unit-testable; the runner is the integration point
//! that turns [`NotifEvent`]s into [`crate::AppMsg`] variants.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use la_ipc::transport::{connect, Endpoint};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request};
use la_proto::methods::{EventTopic, EventsSubscribeParams};
use la_proto::notifications::{
    BackendHealth as WireBackendHealth, CronFired, CronFiredParams, DaemonHealth,
    DaemonHealthParams, NotificationMethod,
};

use crate::BackendBadge;

/// Compact projection of `daemon.health` for the status bar. Drops the
/// per-backend payload (the runner forwards that separately as
/// [`NotifEvent::Backends`]) and keeps only the scalar counters the bar
/// reads directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub running: u32,
    pub queue_depth: u32,
    pub errors_last_5m: u32,
}

/// Outbound event from the subscriber thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifEvent {
    /// Per-backend probe snapshot. Same payload `health_sub` shipped
    /// before WEK-36 — kept under its own variant so the runner can
    /// keep dispatching it as [`crate::AppMsg::BackendsUpdate`]
    /// untouched.
    Backends(Vec<BackendBadge>),
    /// Scalar counters from the same `daemon.health` notification:
    /// running session count + errors-last-5m. Drives the status bar.
    Health(HealthSnapshot),
    /// One cron firing pulse — bar flashes a `↻ cron-id` badge.
    CronFired(CronFiredParams),
    /// The IPC pump lost its connection (any error: handshake failure,
    /// EOF, decode error). Emitted exactly once per disconnect so the
    /// status bar can flip the daemon dot red within one frame. The
    /// pump then sleeps on backoff and retries; the next successful
    /// `Health`/`Backends` push implies the daemon is back.
    DaemonOffline,
}

/// Back-compat alias for the pre-WEK-36 `HealthEvent` name. Lets the
/// existing render tests + old call sites keep compiling without
/// rewriting; new code should prefer [`NotifEvent`].
pub type HealthEvent = NotifEvent;

/// Spawn the daemon-notification subscriber on a dedicated OS thread.
///
/// Returns the receive end of a sync channel the runner drains on every
/// tick. The thread owns its tokio runtime and the daemon connection;
/// it exits when the channel's sender is dropped (the runner detects EOF
/// via `try_recv` returning `Disconnected`).
///
/// On connection failure the thread emits [`NotifEvent::DaemonOffline`]
/// and sleeps on [`reconnect_backoff`] before retrying — so a daemon
/// restart re-establishes the subscription without restarting `la`.
pub fn spawn(socket: &Path) -> Receiver<NotifEvent> {
    spawn_with_config(socket, ReconnectConfig::default())
}

/// Tunables for the reconnect loop. Tests use shorter delays via
/// [`spawn_with_config`]; production calls [`spawn`] which uses the
/// default.
#[derive(Debug, Clone, Copy)]
pub struct ReconnectConfig {
    /// First sleep after a disconnect.
    pub initial_backoff: Duration,
    /// Hard ceiling on the per-attempt sleep.
    pub max_backoff: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(10),
        }
    }
}

/// Spawn the subscriber with a custom backoff policy. Production code
/// should use [`spawn`]; the integration test in `notif_sub_live.rs`
/// uses this entry to keep reconnect under a tenth of a second so the
/// test doesn't add seconds to the suite.
pub fn spawn_with_config(socket: &Path, cfg: ReconnectConfig) -> Receiver<NotifEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    let socket = socket.to_path_buf();
    std::thread::Builder::new()
        .name("la-notif-sub".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::warn!(%err, "notif-sub: failed to build tokio runtime");
                    return;
                }
            };
            rt.block_on(async move { reconnect_loop(socket, tx, cfg).await });
        })
        .expect("spawn la-notif-sub thread");
    rx
}

async fn reconnect_loop(socket: PathBuf, tx: Sender<NotifEvent>, cfg: ReconnectConfig) {
    let mut backoff = cfg.initial_backoff;
    loop {
        match run_once(&socket, &tx).await {
            Ok(()) => {
                // `run_once` only returns Ok(()) when the receiver was
                // dropped — runner shutdown, no need to reconnect.
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "notif-sub: connection ended, reconnecting");
                // Notify the renderer immediately so the daemon dot
                // flips red within one frame. If the channel is dead the
                // runner has shut down — bail out instead of looping
                // forever.
                if tx.send(NotifEvent::DaemonOffline).is_err() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(cfg.max_backoff);
            }
        }
    }
}

async fn run_once(socket: &Path, tx: &Sender<NotifEvent>) -> Result<(), String> {
    let endpoint = endpoint_for(socket);
    let stream = tokio::time::timeout(Duration::from_secs(2), connect(&endpoint))
        .await
        .map_err(|_| format!("timed out connecting to {}", socket.display()))?
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let mut conn = Connection::new(stream);
    let _info = client_handshake(
        &mut conn,
        "la-notif-sub",
        env!("CARGO_PKG_VERSION"),
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .map_err(|e| format!("handshake: {e}"))?;

    // Subscribe to both topics in a single round-trip. Pre-WEK-36 only
    // DaemonHealth was requested; the dispatcher accepts unknown topics
    // by silently dropping them (see EventsSubscribeResult.topics) so
    // older daemons that lack CronFired support still work — the
    // status bar simply never shows a `↻` pulse.
    let params = EventsSubscribeParams {
        topics: vec![EventTopic::DaemonHealth, EventTopic::CronFired],
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
            // Responses to the subscribe request go here; any unsolicited
            // Request is unexpected but harmless.
            _ => continue,
        };
        let Some(params) = n.params else { continue };
        match n.method.as_str() {
            DaemonHealth::NAME => {
                let payload: DaemonHealthParams = match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::warn!(%err, "notif-sub: decode daemon.health failed");
                        continue;
                    }
                };
                // Send Backends first so the sidebar grey-state and the
                // status bar move on the same frame — keeps the UI
                // coherent for an observer watching one cell change.
                let badges = wire_to_badges(&payload.backends);
                let snap = HealthSnapshot {
                    running: payload.running,
                    queue_depth: payload.queue_depth,
                    errors_last_5m: payload.errors_last_5m,
                };
                if tx.send(NotifEvent::Backends(badges)).is_err()
                    || tx.send(NotifEvent::Health(snap)).is_err()
                {
                    // Receiver dropped — runner shutdown.
                    return Ok(());
                }
            }
            CronFired::NAME => {
                let payload: CronFiredParams = match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::warn!(%err, "notif-sub: decode cron.fired failed");
                        continue;
                    }
                };
                if tx.send(NotifEvent::CronFired(payload)).is_err() {
                    return Ok(());
                }
            }
            _ => continue,
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
