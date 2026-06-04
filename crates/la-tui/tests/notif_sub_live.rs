//! WEK-36 / M3.5 status-bar integration tests.
//!
//! Two acceptance criteria are exercised end-to-end against a stub
//! `lad` listening on a UDS:
//!
//! 1. `events.subscribe` requests both `daemon.health` AND `cron.fired`,
//!    and the pump forwards each push as the right [`NotifEvent`]
//!    variant (Backends + Health + CronFired) up the runner channel.
//! 2. After the daemon closes the connection, the pump emits
//!    `DaemonOffline` AND reconnects: a second `events.subscribe`
//!    arrives on the new connection, and the next pushed
//!    `daemon.health` is delivered. This is what makes the WEK-36
//!    "daemon restart 状态栏自动恢复" acceptance true without
//!    restarting `la`.
//!
//! The stub uses `la-ipc` directly so we don't pull `la-daemon` into
//! la-tui dev-deps.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use la_ipc::transport::{Endpoint, Listener};
use la_ipc::{server_handshake, Connection};
use la_proto::jsonrpc::{Message, Notification, Response};
use la_proto::methods::{EventTopic, EventsSubscribeResult, ServerCapabilities};
use la_proto::notifications::{
    BackendHealth, BackendHealthStatus, CronFired, CronFiredParams, DaemonHealth,
    DaemonHealthParams, NotificationMethod,
};
use la_tui::notif_sub::{spawn_with_config, NotifEvent, ReconnectConfig};

const SUBSCRIBE_EXPECTED_TOPICS: &[EventTopic] = &[EventTopic::DaemonHealth, EventTopic::CronFired];

/// Drive one "fake `lad`" session: handshake, accept `events.subscribe`,
/// push the supplied notifications, then either keep the connection
/// open (for the steady-state test) or hang up (for the reconnect
/// test).
async fn run_stub_once(
    listener: &Listener,
    pushes: Vec<Message>,
    keep_alive: bool,
) -> Vec<EventTopic> {
    let stream = listener.accept().await.expect("accept");
    let mut conn = Connection::new(stream);
    let caps = ServerCapabilities {
        adapters: vec!["codex".into()],
        cron: true,
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

    let msg = conn.recv().await.expect("recv").expect("eof");
    let (sub_id, topics) = match msg {
        Message::Request(r) => {
            assert_eq!(r.method, "events.subscribe");
            let params: la_proto::methods::EventsSubscribeParams =
                serde_json::from_value(r.params.expect("subscribe params"))
                    .expect("decode subscribe");
            (r.id, params.topics)
        }
        other => panic!("expected events.subscribe request, got {other:?}"),
    };
    let resp = Response::success(
        sub_id,
        &EventsSubscribeResult {
            topics: topics.clone(),
        },
    )
    .expect("encode sub result");
    conn.send(&Message::Response(resp)).await.expect("send ack");

    for push in pushes {
        conn.send(&push).await.expect("send push");
    }

    if keep_alive {
        // Park until the client drops; recv returns Ok(None) on EOF.
        let _ = conn.recv().await;
    }
    // Dropping `conn` closes the stream, simulating a daemon stop.

    topics
}

fn health_push() -> Message {
    let payload = DaemonHealthParams {
        queue_depth: 0,
        running: 3,
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
        managed_by: None,
    };
    Message::Notification(Notification::new(DaemonHealth::NAME, &payload).expect("encode health"))
}

fn cron_push() -> Message {
    let payload = CronFiredParams {
        cron_id: "nightly-review".into(),
        run_id: "r-1".into(),
        fired_at: "2026-06-03T02:00:00Z".into(),
        status: "spawning".into(),
    };
    Message::Notification(Notification::new(CronFired::NAME, &payload).expect("encode cron"))
}

/// Drain `rx` synchronously until `predicate` returns true or the
/// deadline passes. The receiver is owned by the closure so this is
/// safe to call from `spawn_blocking`. Returns the events seen up to
/// and including the one that satisfied the predicate.
fn drain_until(
    rx: Receiver<NotifEvent>,
    deadline: Instant,
    mut predicate: impl FnMut(&[NotifEvent]) -> bool,
) -> Vec<NotifEvent> {
    let mut out = Vec::new();
    loop {
        if predicate(&out) {
            return out;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return out;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(ev) => out.push(ev),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return out,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_routes_health_and_cron_pushes_to_runner_channel() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-1.sock");

    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");
    let server = tokio::spawn(async move {
        // Close after the pushes so the test thread can join the stub
        // quickly. The pump will try to reconnect; the rest of the
        // test doesn't care because we own the listener and don't
        // re-accept.
        run_stub_once(&listener, vec![health_push(), cron_push()], false).await
    });

    let rx = spawn_with_config(
        &socket,
        ReconnectConfig {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(100),
        },
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let events = tokio::task::spawn_blocking(move || {
        let collected = drain_until(rx, deadline, |seen| {
            let backends = seen.iter().any(|e| matches!(e, NotifEvent::Backends(_)));
            let health = seen.iter().any(|e| matches!(e, NotifEvent::Health(_)));
            let cron = seen.iter().any(|e| matches!(e, NotifEvent::CronFired(_)));
            backends && health && cron
        });
        // Drop the receiver inside this thread so the subscriber pump
        // observes the disconnect and the stub server can exit its
        // `conn.recv()` park. Returning `collected` while `rx` still
        // lives would leave the stub blocked.
        // (`rx` was already moved into the closure; falling out of
        // scope here drops it.)
        collected
    })
    .await
    .expect("blocking join");

    let mut got_backends = false;
    let mut got_health = false;
    let mut got_cron = false;
    for ev in events {
        match ev {
            NotifEvent::Backends(badges) => {
                assert_eq!(badges.len(), 1);
                assert_eq!(badges[0].id, "codex");
                got_backends = true;
            }
            NotifEvent::Health(h) => {
                assert_eq!(h.running, 3);
                assert_eq!(h.errors_last_5m, 1);
                got_health = true;
            }
            NotifEvent::CronFired(p) => {
                assert_eq!(p.cron_id, "nightly-review");
                got_cron = true;
            }
            NotifEvent::DaemonOffline => {
                // Possible during shutdown — ignore.
            }
        }
    }
    assert!(got_backends, "Backends event never arrived");
    assert!(got_health, "Health event never arrived");
    assert!(got_cron, "CronFired event never arrived");

    let topics = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server join")
        .expect("server task");
    assert_eq!(topics, SUBSCRIBE_EXPECTED_TOPICS);
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pump_reconnects_after_daemon_drop_and_redelivers_health() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-2.sock");

    // Bind the listener once; serve two connections in sequence on the
    // same socket so the pump re-discovers the daemon under the same
    // path post-reconnect.
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");
    let server = tokio::spawn(async move {
        // First connection: push one health, then close — the pump
        // will surface DaemonOffline and try to reconnect on backoff.
        let topics1 = run_stub_once(&listener, vec![health_push()], false).await;
        // Second connection: subscribe again, push one health, then
        // close. Closing (rather than parking) lets the stub task
        // join quickly even if the test's drop sequence races us.
        let topics2 = run_stub_once(&listener, vec![health_push()], false).await;
        (topics1, topics2)
    });

    let rx = spawn_with_config(
        &socket,
        ReconnectConfig {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(100),
        },
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let events = tokio::task::spawn_blocking(move || {
        // Drain inside the spawn_blocking so we own (and ultimately
        // drop) the receiver here. Dropping it tells the pump to
        // shut down and lets the stub server's `conn.recv()` park
        // observe EOF.
        drain_until(rx, deadline, |seen| {
            let pulses = seen
                .iter()
                .filter(|e| matches!(e, NotifEvent::Health(_)))
                .count();
            let offline = seen.iter().any(|e| matches!(e, NotifEvent::DaemonOffline));
            pulses >= 2 && offline
        })
    })
    .await
    .expect("blocking join");

    let pulses = events
        .iter()
        .filter(|e| matches!(e, NotifEvent::Health(_)))
        .count();
    let offline = events
        .iter()
        .any(|e| matches!(e, NotifEvent::DaemonOffline));
    assert!(
        offline,
        "pump never reported DaemonOffline after daemon dropped"
    );
    assert!(
        pulses >= 2,
        "pump only delivered {pulses}/2 health pulses (no reconnect)"
    );

    let (topics1, topics2) = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server join")
        .expect("server task");
    assert_eq!(topics1, SUBSCRIBE_EXPECTED_TOPICS);
    assert_eq!(topics2, SUBSCRIBE_EXPECTED_TOPICS);
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

/// Review fix for WEK-36: after a connection successfully establishes
/// (handshake + `events.subscribe` ack), the next disconnect must reset
/// the reconnect backoff to `initial_backoff` instead of carrying over
/// whatever value the previous outage left. The scenario:
///
///   1. No listener bound → pump fails to connect ≥3 times → backoff
///      grows toward `max_backoff` (300 ms in this test).
///   2. Listener appears → pump establishes the connection, gets one
///      `health` push, then the stub drops the connection.
///   3. The pump should reconnect to the second stub session well under
///      `max_backoff` — the test asserts the second `Health` lands
///      within ~150 ms of the first `DaemonOffline`, which is only
///      possible if the backoff was reset to ~20 ms after the first
///      successful subscribe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_backoff_resets_after_a_successful_connect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-3.sock");

    // Start the pump BEFORE the listener exists so it burns several
    // reconnect attempts. With initial=20ms / max=300ms, four failed
    // attempts push backoff to ~160ms; six pushes it to the 300ms
    // ceiling. Without the reset fix the second reconnect would still
    // wait that long.
    let cfg = ReconnectConfig {
        initial_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(300),
    };
    let rx = spawn_with_config(&socket, cfg);

    // Let the pump fail several times so the backoff is well above
    // initial. 1s ≫ (20 + 40 + 80 + 160 + 300 + 300) ≈ 900ms worth of
    // sleeps, so backoff is definitely at the 300ms ceiling.
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");
    let server = tokio::spawn(async move {
        // First successful connection: push one health, then close.
        run_stub_once(&listener, vec![health_push()], false).await;
        // Second connection: pump must reconnect after the first close.
        // With backoff reset, the gap should be ~initial_backoff (20ms)
        // not ~max_backoff (300ms).
        run_stub_once(&listener, vec![health_push()], false).await;
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let timing = tokio::task::spawn_blocking(move || {
        let mut first_offline_at: Option<Instant> = None;
        let mut second_health_at: Option<Instant> = None;
        let mut health_count = 0usize;
        loop {
            if let (Some(off), Some(h2)) = (first_offline_at, second_health_at) {
                return Some(h2.duration_since(off));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(NotifEvent::Health(_)) => {
                    health_count += 1;
                    if health_count == 2 && second_health_at.is_none() {
                        second_health_at = Some(Instant::now());
                    }
                }
                Ok(NotifEvent::DaemonOffline) => {
                    // We care about the FIRST offline that follows a
                    // successful Health push, not the early failures
                    // before the listener existed.
                    if health_count >= 1 && first_offline_at.is_none() {
                        first_offline_at = Some(Instant::now());
                    }
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    })
    .await
    .expect("blocking join");

    let gap = timing.expect("never saw both first DaemonOffline and second Health");
    // Allow generous slack for CI jitter — 150ms is still half of
    // max_backoff and well above the ~20ms initial. If the reset
    // regressed we'd expect ~300ms here and the assert would fire.
    assert!(
        gap < Duration::from_millis(200),
        "second reconnect took {gap:?}; expected ≪ max_backoff (300ms). \
         backoff likely did not reset after the first successful connect."
    );

    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}
