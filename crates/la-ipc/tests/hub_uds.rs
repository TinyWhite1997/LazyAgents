//! Integration tests for the M1.2 multi-attach + backpressure + reconnect
//! transport, running over real Unix Domain Sockets.
//!
//! The mock-daemon pattern mirrors `tests/round_trip.rs`: a tokio task
//! holds an [`OutputHub`], spawns one writer task per attached connection,
//! and answers `sessions.attach` from the (also-mocked) client. The real
//! daemon (M1.7) will look very similar — wiring this together early
//! catches contract drift between the hub and the connection layer before
//! the daemon assembly task starts.
//!
//! These tests are the acceptance harness named in WEK-16:
//!   - kill a client < 30s then reconnect → no lost bytes
//!   - slow client (1 KiB/s) → gap notice, others unaffected

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use la_ipc::hub::{HubConfig, HubEvent, OutputHub, SubId};
use la_ipc::transport::{connect, Endpoint, Listener};
use la_ipc::{client_handshake, server_handshake, Connection, SendHalf};
use la_proto::jsonrpc::{Message, Notification, Request, Response, ResponseOutcome};
use la_proto::methods::{
    Method, ServerCapabilities, SessionsAttach, SessionsAttachParams, SessionsAttachResult,
};
use la_proto::notifications::{NotificationMethod, SessionGap, SessionOutput};
use la_proto::PROTOCOL_VERSION;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// Spawn a mock daemon backed by an `OutputHub`. The daemon answers
/// `sessions.attach` (using the wire-level `resume_from_seq` field to
/// rebind a parked subscription, matching the real daemon's behaviour
/// after WEK-49) and then drives the connection's writer task. Returns
/// the publisher handle, the temp dir (to keep the socket alive), and
/// the listener task.
async fn spawn_hub_daemon(
    hub: OutputHub,
    park_after_drop: Arc<Mutex<Option<SubId>>>,
) -> (TempDir, std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let dir = TempDir::new().expect("tempdir");
    let sock = dir.path().join("lad.sock");
    let listener = Listener::bind(&Endpoint::uds(&sock)).await.expect("bind");

    let sock_clone = sock.clone();
    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let hub = hub.clone();
            let park_after_drop = park_after_drop.clone();
            tokio::spawn(async move {
                handle_one_conn(hub, stream, park_after_drop).await;
            });
        }
    });
    (dir, sock_clone, task)
}

async fn handle_one_conn(
    hub: OutputHub,
    stream: UnixStream,
    park_after_drop: Arc<Mutex<Option<SubId>>>,
) {
    let mut conn = Connection::new(stream);
    let _ = server_handshake(
        &mut conn,
        "lad",
        "0.4.1",
        &[PROTOCOL_VERSION],
        ServerCapabilities {
            adapters: vec!["claude".into()],
            cron: false,
            worktree: false,
            events: false,
        },
    )
    .await
    .expect("server handshake");

    // Expect exactly one sessions.attach. The wire-level `resume_from_seq`
    // (WEK-49) carries the "since seq" for reconnects; first attaches pass
    // `None`, which is live-only (no ring replay) — only `Some(prev_seq)`
    // asks the hub to replay seq > prev_seq.
    let msg = conn.recv().await.expect("io").expect("eof on attach");
    let Message::Request(req) = msg else { panic!() };
    assert_eq!(req.method, SessionsAttach::NAME);
    let p: SessionsAttachParams = req.params_as().expect("attach params");
    let since = p.resume_from_seq;

    let parked_id = park_after_drop.lock().await.take();
    let subscription = if let Some(id) = parked_id {
        if let Some(s) = hub.resume(id, since).await {
            s
        } else {
            hub.subscribe(since).await
        }
    } else {
        hub.subscribe(since).await
    };

    let resp = Response::success(
        req.id,
        &SessionsAttachResult {
            session_id: hub.session_id().into(),
            snapshot_seq: since.unwrap_or(0),
            input_acquired: p.acquire_input,
            sub_token: None,
        },
    )
    .expect("encode");
    conn.send(&Message::Response(resp)).await.expect("send");

    // Remember the SubId for a follow-up attach to "resume" it.
    *park_after_drop.lock().await = Some(subscription.id());

    let (send_half, mut recv_half) = conn.split();
    let send_half = Arc::new(send_half);

    // Writer: pump hub events out as notifications.
    let writer = tokio::spawn(async move {
        run_writer(send_half, subscription).await;
    });

    // Reader: wait for EOF / disconnect. We don't expect more requests.
    while let Ok(Some(_)) = recv_half.recv().await {}
    writer.abort();
    let _ = writer.await;
}

async fn run_writer<S>(send: Arc<SendHalf<S>>, mut sub: la_ipc::Subscription)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
{
    while let Some(ev) = sub.recv().await {
        let msg = match ev {
            HubEvent::Output(p) => {
                let n = Notification::new(SessionOutput::NAME, &*p).expect("encode out");
                Message::Notification(n)
            }
            HubEvent::Gap(p) => {
                let n = Notification::new(SessionGap::NAME, &p).expect("encode gap");
                Message::Notification(n)
            }
        };
        if send.send(&msg).await.is_err() {
            break;
        }
    }
}

/// WEK-16 acceptance #1: kill client < 30s, reconnect with seq → no bytes lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_and_reconnect_within_grace_loses_no_bytes() {
    let hub = OutputHub::with_config(
        "sid",
        HubConfig {
            // 1 s park window so the test isn't slow; the production
            // default is 30 s but the property under test is identical.
            park_duration: Duration::from_secs(1),
            ..HubConfig::default()
        },
    );
    let pid_slot = Arc::new(Mutex::new(None));
    let (_dir, sock, _accept) = spawn_hub_daemon(hub.clone(), pid_slot.clone()).await;

    // === First client ===
    let stream = connect(&Endpoint::uds(&sock)).await.unwrap();
    let mut conn = Connection::new(stream);
    let _ = client_handshake(&mut conn, "la", "0.4.1", &[PROTOCOL_VERSION])
        .await
        .unwrap();
    let req = Request::new(
        2,
        SessionsAttach::NAME,
        SessionsAttachParams {
            session_id: "sid".into(),
            resume_from_seq: None,
            replay_bytes: None,
            acquire_input: false,
        },
    )
    .unwrap();
    conn.send(&Message::Request(req)).await.unwrap();
    match conn.recv().await.unwrap().unwrap() {
        Message::Response(Response {
            outcome: ResponseOutcome::Result(_),
            ..
        }) => (),
        other => panic!("attach: {other:?}"),
    };

    // Publish 3 chunks and drain them.
    for i in 0..3 {
        hub.publish(format!("first-{i}").as_bytes()).await;
    }
    let mut got = Vec::new();
    for _ in 0..3 {
        let m = tokio::time::timeout(Duration::from_secs(2), conn.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Notification(n) = m {
            let p: la_proto::notifications::SessionOutputParams = n.params_as().unwrap();
            got.push((p.seq, p.data_bytes().unwrap()));
        }
    }
    let last_seq = got.last().unwrap().0;

    // === Abrupt disconnect: drop the socket from under the daemon ===
    drop(conn);
    // Give the daemon a moment to notice EOF and park the subscription.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Publisher keeps going while we're "away" — these must survive in the
    // ring + per-sub queue so the resumed reader gets them.
    for i in 0..4 {
        hub.publish(format!("during-gap-{i}").as_bytes()).await;
    }

    // === Reconnect within the park window ===
    let stream = connect(&Endpoint::uds(&sock)).await.unwrap();
    let mut conn = Connection::new(stream);
    let _ = client_handshake(&mut conn, "la", "0.4.1", &[PROTOCOL_VERSION])
        .await
        .unwrap();
    // Pass last_seq as resume_from_seq — the canonical reconnect path
    // (WEK-49) replacing the M1.2-era replay_bytes overload.
    let req = Request::new(
        3,
        SessionsAttach::NAME,
        SessionsAttachParams {
            session_id: "sid".into(),
            resume_from_seq: Some(last_seq),
            replay_bytes: None,
            acquire_input: false,
        },
    )
    .unwrap();
    conn.send(&Message::Request(req)).await.unwrap();
    let _ = conn.recv().await.unwrap().unwrap();

    // Drain the catch-up. No Gap allowed.
    let mut resumed = Vec::new();
    for _ in 0..4 {
        let m = tokio::time::timeout(Duration::from_secs(2), conn.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Message::Notification(n) = m else {
            panic!()
        };
        assert_eq!(
            n.method,
            SessionOutput::NAME,
            "resume after grace must not emit a gap"
        );
        let p: la_proto::notifications::SessionOutputParams = n.params_as().unwrap();
        resumed.push((p.seq, p.data_bytes().unwrap()));
    }
    assert_eq!(resumed.len(), 4);
    for (i, (_seq, bytes)) in resumed.iter().enumerate() {
        assert_eq!(bytes, format!("during-gap-{i}").as_bytes());
    }
    // Seq must continue strictly increasing from `last_seq + 1`.
    assert_eq!(resumed[0].0, last_seq + 1);
}

/// WEK-16 acceptance #2: a slow client (drains ~1 KiB/s) → gap notice,
/// other clients see every chunk in order with no gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_client_gap_does_not_affect_fast_client() {
    // Tight per-sub queue so the slow path overflows quickly. Production
    // would use 1 MiB; we use 64 KiB here so the test runs in ~1 s.
    let hub = OutputHub::with_config(
        "sid",
        HubConfig {
            ring_bytes: 1024 * 1024,
            sub_queue_bytes: 64 * 1024,
            park_duration: Duration::from_secs(1),
        },
    );
    let park_slot_a = Arc::new(Mutex::new(None));
    let park_slot_b = Arc::new(Mutex::new(None));
    let (_dir_a, sock_a, _accept_a) = spawn_hub_daemon(hub.clone(), park_slot_a).await;
    // Same hub, second listener — gives each client its own socket but a
    // shared session. The production daemon will multiplex over one
    // socket; this test only cares about the fan-out semantics.
    let (_dir_b, sock_b, _accept_b) = spawn_hub_daemon(hub.clone(), park_slot_b).await;

    // --- "Fast" client: drains as fast as it can ---
    let fast = tokio::spawn(async move {
        let stream = connect(&Endpoint::uds(&sock_a)).await.unwrap();
        let mut conn = Connection::new(stream);
        let _ = client_handshake(&mut conn, "la-fast", "0.4.1", &[PROTOCOL_VERSION])
            .await
            .unwrap();
        let req = Request::new(
            1,
            SessionsAttach::NAME,
            SessionsAttachParams {
                session_id: "sid".into(),
                resume_from_seq: None,
                replay_bytes: None,
                acquire_input: false,
            },
        )
        .unwrap();
        conn.send(&Message::Request(req)).await.unwrap();
        let _ = conn.recv().await.unwrap();
        let mut count = 0;
        let mut gap_seen = false;
        let mut total_bytes = 0u64;
        loop {
            let m = tokio::time::timeout(Duration::from_secs(3), conn.recv()).await;
            match m {
                Ok(Ok(Some(Message::Notification(n)))) => {
                    if n.method == SessionOutput::NAME {
                        let p: la_proto::notifications::SessionOutputParams =
                            n.params_as().unwrap();
                        count += 1;
                        total_bytes += p.data_bytes().unwrap().len() as u64;
                    } else if n.method == SessionGap::NAME {
                        gap_seen = true;
                    } else {
                        eprintln!("fast: unexpected notification {}", n.method);
                    }
                }
                Ok(Ok(Some(_other))) => break,
                Ok(Ok(None)) => break,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        (count, total_bytes, gap_seen)
    });

    // --- "Slow" client: opens the socket, attaches, then drains at ~1
    //     chunk / 100 ms which is comfortably below the publisher's rate.
    //     Just opens raw stream + handshake; we don't even drive a writer
    //     task so the socket-side buffer also fills, but the *hub* side
    //     queue is the cap we test against (4 KiB) ---
    let slow = tokio::spawn(async move {
        let stream = connect(&Endpoint::uds(&sock_b)).await.unwrap();
        let mut conn = Connection::new(stream);
        let _ = client_handshake(&mut conn, "la-slow", "0.4.1", &[PROTOCOL_VERSION])
            .await
            .unwrap();
        let req = Request::new(
            1,
            SessionsAttach::NAME,
            SessionsAttachParams {
                session_id: "sid".into(),
                resume_from_seq: None,
                replay_bytes: None,
                acquire_input: false,
            },
        )
        .unwrap();
        conn.send(&Message::Request(req)).await.unwrap();
        let _ = conn.recv().await.unwrap();
        let mut gap_count = 0;
        let mut dropped = 0u64;
        let mut outputs = 0;
        loop {
            let m = tokio::time::timeout(Duration::from_secs(3), conn.recv()).await;
            // Deliberately slow drain: sleep 100ms per message.
            tokio::time::sleep(Duration::from_millis(100)).await;
            match m {
                Ok(Ok(Some(Message::Notification(n)))) if n.method == SessionGap::NAME => {
                    let p: la_proto::notifications::SessionGapParams = n.params_as().unwrap();
                    gap_count += 1;
                    dropped += p.dropped_bytes;
                }
                Ok(Ok(Some(Message::Notification(_)))) => outputs += 1,
                _ => break,
            }
        }
        (gap_count, dropped, outputs)
    });

    // Give both clients time to attach.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Publisher: 1 MiB of payload in 64 chunks of 16 KiB each. Sizing
    // intuition:
    //   - The kernel's loopback UDS socket buffer is in the low hundreds
    //     of KiB on Linux, so a slow reader that never drains will block
    //     its writer task only after the buffer fills.
    //   - Once the writer blocks on `send()`, the hub-level 4 KiB queue
    //     fills next, which is what triggers the gap notice we want to
    //     observe.
    //   - The fast reader drains continuously, so its socket buffer never
    //     fills and its hub queue stays empty regardless of total volume.
    // Publisher: 64 chunks of 4 KiB. Sizing intuition:
    //   - Fast client drains continuously; its hub queue stays well below
    //     the 64 KiB cap (a single chunk is 4 KiB).
    //   - Slow client sleeps 100 ms per recv → the kernel's UDS socket
    //     buffer fills first, blocking the writer task's `send`, which
    //     stops draining the hub queue. The hub queue then fills past
    //     64 KiB and the producer starts emitting Gap notices.
    let payload = vec![b'p'; 4 * 1024];
    for _ in 0..64 {
        hub.publish(&payload).await;
        // Pace publication slightly below the fast writer's drain capacity
        // so the fast client never has more than a chunk or two pending.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Wait for both clients to settle. The fast one will go idle quickly;
    // the slow one is the long pole.
    let (fast_count, fast_bytes, fast_gap) = fast.await.unwrap();
    let (slow_gap_count, slow_dropped, slow_outputs) = slow.await.unwrap();

    assert!(
        !fast_gap,
        "fast client should not have seen a gap (counts: outputs={fast_count}, bytes={fast_bytes})"
    );
    assert_eq!(
        fast_count, 64,
        "fast client must receive every chunk (got {fast_count})"
    );
    assert_eq!(fast_bytes, 64 * 4 * 1024, "fast client byte total mismatch");
    assert!(
        slow_gap_count >= 1,
        "slow client must see at least one gap notice (got 0; outputs={slow_outputs}, dropped={slow_dropped})"
    );
    assert!(
        slow_dropped > 0,
        "slow client gap must report dropped bytes"
    );
}

/// Make sure the explicit shutdown path is covered: an abrupt connection
/// close that does NOT reconnect within the grace eventually evicts the
/// subscription. (Sanity check for the park-deadline path in production —
/// otherwise leaked subscriptions would balloon hub state.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_reconnect_within_grace_evicts_subscription() {
    let hub = OutputHub::with_config(
        "sid",
        HubConfig {
            park_duration: Duration::from_millis(80),
            ..HubConfig::default()
        },
    );
    let pid_slot = Arc::new(Mutex::new(None));
    let (_dir, sock, _) = spawn_hub_daemon(hub.clone(), pid_slot.clone()).await;

    // Attach a client and disconnect immediately.
    let mut stream = connect(&Endpoint::uds(&sock)).await.unwrap();
    {
        let mut conn = Connection::new(&mut stream);
        let _ = client_handshake(&mut conn, "la", "0.4.1", &[PROTOCOL_VERSION])
            .await
            .unwrap();
        let req = Request::new(
            1,
            SessionsAttach::NAME,
            SessionsAttachParams {
                session_id: "sid".into(),
                resume_from_seq: None,
                replay_bytes: None,
                acquire_input: false,
            },
        )
        .unwrap();
        conn.send(&Message::Request(req)).await.unwrap();
        let _ = conn.recv().await.unwrap();
    }
    let _ = stream.shutdown().await;
    drop(stream);

    // Walk the timeline: parked → evicted.
    let id = loop {
        if let Some(id) = *pid_slot.lock().await {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    // Wait past the park deadline.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let evicted = hub.evict_if_still_parked(id).await;
    assert!(
        evicted,
        "subscription should be evicted past its park deadline"
    );
    assert!(
        hub.resume(id, None).await.is_none(),
        "evicted id must not be resumable"
    );
}
