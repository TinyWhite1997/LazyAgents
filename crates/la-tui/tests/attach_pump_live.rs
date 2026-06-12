// Uses `la_ipc::transport::Listener::bind` with a UDS endpoint, which
// returns `Unsupported "UDS is not available on Windows"` at runtime.
// Gating the file to unix keeps WEK-72's matrix CI green on
// windows-2022; the Windows transport has its own coverage.
#![cfg(unix)]

//! WEK-92-A3 acceptance for the per-session attach pump.
//!
//! Drives a stub `lad` listening on a UDS through the full attach
//! contract:
//!   1. Pump issues `sessions.attach { acquire_input: true }`.
//!   2. Pump forwards `session.output` notifications as
//!      [`AttachEvent::Bytes`] up the runner channel.
//!   3. Pump forwards typed bytes as `sessions.write` requests.
//!   4. When the daemon drops the connection the pump emits
//!      `Disconnected { will_reconnect: true }` AND reconnects, calling
//!      `sessions.attach` again with `resume_from_seq = Some(last_seq)`
//!      so the catch-up doesn't double-deliver — mirroring the
//!      `reattach_with_resume_from_seq_catches_up_without_double_delivery`
//!      contract from WEK-49.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use la_ipc::transport::{Endpoint, Listener};
use la_ipc::{server_handshake, Connection};
use la_proto::jsonrpc::{Message, Notification, Response};
use la_proto::methods::{
    ServerCapabilities, SessionsAttachParams, SessionsAttachResult, SessionsDetachResult,
    SessionsWriteParams, SessionsWriteResult,
};
use la_proto::notifications::{NotificationMethod, SessionOutput, SessionOutputParams};
use la_tui::attach_pump::{AttachCommand, AttachEvent, AttachPump};

const SESSION_ID: &str = "sess-xyz";

fn output_push(seq: u64, payload: &[u8]) -> Message {
    let params = SessionOutputParams::from_bytes(SESSION_ID, seq, payload);
    Message::Notification(Notification::new(SessionOutput::NAME, &params).expect("encode output"))
}

/// Outcome of one full stub session — what the test wants to assert on.
#[derive(Debug, Default)]
struct StubResult {
    received_resume_from_seq: Option<u64>,
    received_acquire_input: bool,
    received_writes: Vec<Vec<u8>>,
    detached: bool,
}

/// Drive one stub `lad` session: handshake, accept `sessions.attach`,
/// push the supplied outputs, then process inbound writes / detach
/// until either the configured budget elapses or the client disconnects.
async fn run_stub_once(
    listener: &Listener,
    pushes_before_writes: Vec<Message>,
    process_for: Duration,
) -> StubResult {
    run_stub_with_order(listener, Vec::new(), pushes_before_writes, process_for).await
}

/// Same as [`run_stub_once`] but lets the caller send a batch of
/// `session.*` notifications BEFORE the attach response is written
/// back. The daemon's dispatcher legitimately produces this ordering
/// (subscription published + writer task notified before the
/// SessionsAttachResult is serialized — see
/// `crates/la-daemon/src/dispatcher.rs:handle_sessions_attach`), so the
/// pump must NOT drop the pre-ack frames.
async fn run_stub_with_order(
    listener: &Listener,
    pushes_before_ack: Vec<Message>,
    pushes_after_ack: Vec<Message>,
    process_for: Duration,
) -> StubResult {
    let stream = listener.accept().await.expect("accept");
    let mut conn = Connection::new(stream);
    let caps = ServerCapabilities {
        adapters: vec!["shtest".into()],
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

    let attach_req = match conn.recv().await.expect("recv").expect("eof") {
        Message::Request(r) => r,
        other => panic!("expected sessions.attach request, got {other:?}"),
    };
    assert_eq!(attach_req.method, "sessions.attach");
    let params: SessionsAttachParams =
        serde_json::from_value(attach_req.params.expect("attach params"))
            .expect("decode SessionsAttachParams");
    let mut result = StubResult {
        received_resume_from_seq: params.resume_from_seq,
        received_acquire_input: params.acquire_input,
        ..Default::default()
    };
    let ack = Response::success(
        attach_req.id,
        &SessionsAttachResult {
            session_id: params.session_id.clone(),
            snapshot_seq: params.resume_from_seq.unwrap_or(0),
            input_acquired: params.acquire_input,
            sub_token: None,
        },
    )
    .expect("encode ack");

    // Pre-ack notifications (regression coverage for the daemon
    // ordering where session.output lands before SessionsAttachResult).
    for push in pushes_before_ack {
        conn.send(&push).await.expect("send pre-ack push");
    }
    conn.send(&Message::Response(ack))
        .await
        .expect("send attach ack");

    for push in pushes_after_ack {
        conn.send(&push).await.expect("send push");
    }

    let deadline = tokio::time::Instant::now() + process_for;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let frame = match tokio::time::timeout(remaining, conn.recv()).await {
            Ok(Ok(Some(m))) => m,
            Ok(Ok(None)) => break,
            Ok(Err(_)) => break,
            Err(_) => break,
        };
        match frame {
            Message::Request(req) => match req.method.as_str() {
                "sessions.write" => {
                    let p: SessionsWriteParams =
                        serde_json::from_value(req.params.expect("write params"))
                            .expect("decode write");
                    let bytes = p.data_bytes().expect("base64 write");
                    result.received_writes.push(bytes);
                    let ack = Response::success(req.id, &SessionsWriteResult::default())
                        .expect("encode write ack");
                    let _ = conn.send(&Message::Response(ack)).await;
                }
                "sessions.detach" => {
                    result.detached = true;
                    let ack = Response::success(req.id, SessionsDetachResult::default())
                        .expect("encode detach ack");
                    let _ = conn.send(&Message::Response(ack)).await;
                    break;
                }
                other => panic!("unexpected request method: {other}"),
            },
            Message::Notification(_) | Message::Response(_) => continue,
        }
    }
    result
}

fn drain_until(
    rx: &Receiver<AttachEvent>,
    deadline: Instant,
    mut predicate: impl FnMut(&[AttachEvent]) -> bool,
) -> Vec<AttachEvent> {
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
async fn attach_forwards_output_and_typed_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-attach-1.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");
    let server = tokio::spawn(async move {
        run_stub_once(
            &listener,
            vec![output_push(1, b"hello "), output_push(2, b"world\n")],
            Duration::from_secs(3),
        )
        .await
    });

    let pump = AttachPump::spawn(&socket, SESSION_ID);
    let tx_for_keystroke = pump.tx.clone();
    let rx = pump.rx;

    let deadline = Instant::now() + Duration::from_secs(5);
    let collected = tokio::task::spawn_blocking(move || {
        // Single drain loop: accumulate every event until we have seen
        // Connected AND the "hello world" bytes, then ask to detach
        // and wait for Closed. Sending writes happens the first time we
        // observe Connected.
        let mut all = Vec::new();
        let mut sent_writes = false;
        let mut sent_detach = false;
        let mut bytes_seen = Vec::new();
        loop {
            if sent_detach && all.iter().any(|e| matches!(e, AttachEvent::Closed)) {
                return all;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return all;
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(ev) => {
                    if matches!(ev, AttachEvent::Connected { .. }) && !sent_writes {
                        let _ = tx_for_keystroke.send(AttachCommand::Write(b"abc".to_vec()));
                        let _ = tx_for_keystroke.send(AttachCommand::Write(vec![b'\r']));
                        sent_writes = true;
                    }
                    if let AttachEvent::Bytes { bytes, .. } = &ev {
                        bytes_seen.extend_from_slice(bytes);
                    }
                    all.push(ev);
                    if !sent_detach && bytes_seen.windows(11).any(|w| w == b"hello world") {
                        let _ = tx_for_keystroke.send(AttachCommand::Detach);
                        sent_detach = true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return all,
            }
        }
    })
    .await
    .expect("blocking join");

    let stub = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server join")
        .expect("server task");

    assert!(stub.received_acquire_input, "pump did not request input");
    assert_eq!(
        stub.received_resume_from_seq,
        Some(0),
        "first attach must request a full ring replay (Some(0)) so a \
         full-screen agent's already-painted UI shows immediately"
    );
    assert!(stub.detached, "pump did not call sessions.detach on stop");
    // Concatenate every byte the stub saw on its write channel and
    // assert the typed sequence landed in order (the pump may split or
    // batch chunks, so we check the concatenation).
    let written: Vec<u8> = stub.received_writes.into_iter().flatten().collect();
    assert!(
        written.windows(3).any(|w| w == b"abc"),
        "stub never saw 'abc' from typed input; got {written:?}"
    );
    assert!(
        written.contains(&b'\r'),
        "stub never saw the carriage return; got {written:?}"
    );

    // Concatenate every byte the pump pushed out and assert the
    // hello-world output landed in order.
    let mut received = Vec::new();
    for ev in collected {
        if let AttachEvent::Bytes { bytes, .. } = ev {
            received.extend_from_slice(&bytes);
        }
    }
    assert!(
        received.windows(11).any(|w| w == b"hello world"),
        "pump never forwarded 'hello world'; got {received:?}"
    );
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pump_reconnects_with_resume_cursor_after_daemon_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-attach-2.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");
    // First connection delivers seq 1+2 then hangs up. Second
    // connection should observe `resume_from_seq = Some(2)` — the last
    // seq the pump saw before the disconnect.
    let server = tokio::spawn(async move {
        let first = run_stub_once(
            &listener,
            vec![output_push(1, b"first "), output_push(2, b"chunk ")],
            // Don't wait for writes/detach — close quickly so the pump
            // observes the disconnect and triggers its one auto-retry.
            Duration::from_millis(150),
        )
        .await;
        let second = run_stub_once(
            &listener,
            vec![output_push(3, b"after-reconnect ")],
            Duration::from_secs(2),
        )
        .await;
        (first, second)
    });

    let pump = AttachPump::spawn(&socket, SESSION_ID);
    let rx = pump.rx;
    let tx = pump.tx.clone();

    let deadline = Instant::now() + Duration::from_secs(6);
    let events = tokio::task::spawn_blocking(move || {
        // Wait until we've seen TWO Connected events (initial + retry).
        let collected = drain_until(&rx, deadline, |seen| {
            let connects = seen
                .iter()
                .filter(|e| matches!(e, AttachEvent::Connected { .. }))
                .count();
            let bytes_seen = seen
                .iter()
                .filter_map(|e| match e {
                    AttachEvent::Bytes { bytes, .. } => Some(bytes.clone()),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<u8>>();
            connects >= 2 && bytes_seen.windows(16).any(|w| w == b"after-reconnect ")
        });
        let _ = tx.send(AttachCommand::Detach);
        collected
    })
    .await
    .expect("blocking join");

    let (first, second) = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server join")
        .expect("server task");

    assert_eq!(
        first.received_resume_from_seq,
        Some(0),
        "first attach must request a full ring replay (Some(0))"
    );
    assert_eq!(
        second.received_resume_from_seq,
        Some(2),
        "reconnect must pass last observed seq as resume cursor"
    );

    let connects = events
        .iter()
        .filter(|e| matches!(e, AttachEvent::Connected { .. }))
        .count();
    let disconnects = events
        .iter()
        .filter(|e| matches!(e, AttachEvent::Disconnected { .. }))
        .count();
    assert!(
        connects >= 2,
        "expected ≥2 Connected events, got {connects}"
    );
    assert!(
        disconnects >= 1,
        "expected ≥1 Disconnected event before the reconnect, got {disconnects}"
    );
    // Concatenate bytes; assert the reconnect chunk landed.
    let mut bytes = Vec::new();
    for ev in &events {
        if let AttachEvent::Bytes { bytes: b, .. } = ev {
            bytes.extend_from_slice(b);
        }
    }
    assert!(
        bytes.windows(16).any(|w| w == b"after-reconnect "),
        "expected reconnect bytes; got {bytes:?}"
    );

    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_ack_notifications_are_replayed_not_dropped() {
    // Regression: the daemon's dispatcher inserts the new subscription
    // into the attachments map and notifies the writer task BEFORE the
    // SessionsAttachResult response is written back. Catch-up bytes
    // can therefore land on the wire before the ack we're waiting for.
    // The pump must buffer those frames and replay them after the
    // Connected event instead of silently dropping them.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-attach-3.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");
    let server = tokio::spawn(async move {
        run_stub_with_order(
            &listener,
            // Pre-ack: simulate two catch-up chunks the daemon emits
            // between subscribing the writer task and serializing the
            // ack. These are what the pre-fix pump dropped.
            vec![output_push(1, b"early-"), output_push(2, b"before-ack ")],
            // Post-ack: ordinary live increment.
            vec![output_push(3, b"after-ack\n")],
            Duration::from_secs(2),
        )
        .await
    });

    let pump = AttachPump::spawn(&socket, SESSION_ID);
    let rx = pump.rx;
    let tx = pump.tx.clone();

    let deadline = Instant::now() + Duration::from_secs(5);
    let events = tokio::task::spawn_blocking(move || {
        let mut all = Vec::new();
        let mut bytes_seen = Vec::new();
        let mut sent_detach = false;
        loop {
            if sent_detach && all.iter().any(|e| matches!(e, AttachEvent::Closed)) {
                return all;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return all;
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(ev) => {
                    if let AttachEvent::Bytes { bytes, .. } = &ev {
                        bytes_seen.extend_from_slice(bytes);
                    }
                    all.push(ev);
                    if !sent_detach && bytes_seen.windows(9).any(|w| w == b"after-ack") {
                        let _ = tx.send(AttachCommand::Detach);
                        sent_detach = true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return all,
            }
        }
    })
    .await
    .expect("blocking join");

    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;

    // Connected must arrive before any Bytes event the App receives —
    // that is the user-visible invariant. The pump achieves it by
    // buffering pre-ack notifications and replaying them after Connected.
    let connected_idx = events
        .iter()
        .position(|e| matches!(e, AttachEvent::Connected { .. }))
        .expect("never saw Connected");
    let first_bytes_idx = events
        .iter()
        .position(|e| matches!(e, AttachEvent::Bytes { .. }))
        .expect("never saw any Bytes — pre-ack notifications were dropped");
    assert!(
        first_bytes_idx > connected_idx,
        "Bytes arrived before Connected (idx {first_bytes_idx} vs {connected_idx}) — \
         buffered notifications must be replayed AFTER the Connected event"
    );

    // Concatenated bytes must contain BOTH the pre-ack chunks and the
    // post-ack chunk, in order.
    let mut received = Vec::new();
    for ev in &events {
        if let AttachEvent::Bytes { bytes, .. } = ev {
            received.extend_from_slice(bytes);
        }
    }
    assert!(
        received.windows(17).any(|w| w == b"early-before-ack "),
        "pre-ack catch-up chunks were dropped; got {received:?}"
    );
    assert!(
        received.windows(9).any(|w| w == b"after-ack"),
        "post-ack chunk missing; got {received:?}"
    );

    let _ = std::fs::remove_file(&socket);
    drop(dir);
}
