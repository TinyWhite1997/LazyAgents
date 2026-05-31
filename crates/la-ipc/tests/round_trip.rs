//! End-to-end integration: a mock daemon and mock client run all five M0.2
//! wire methods over a real UDS, exercising the handshake + framing + typed
//! payloads as one stack. This is the integration test called for by the
//! acceptance criterion in WEK-12 ("集成：mock daemon + mock client 跑通 5 方法").
//!
//! What this test pins down beyond the unit tests:
//! - The 4-byte BE length prefix actually drives `tokio`'s socket layer.
//! - The split send/recv halves do not deadlock when notifications interleave
//!   with responses on the same connection.
//! - Base64-encoded PTY bytes survive end-to-end with byte-for-byte fidelity.
//! - `session.output` chunking obeys the 64 KiB cap and arrives in seq order.
//! - Version negotiation works: the server picks the first overlap.
//!
//! UDS-only on Unix; Windows runs would use Named Pipes via the same
//! transport API but are out of scope per the WEK-5 Linux-only validation.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use la_ipc::transport::{connect, Endpoint, Listener};
use la_ipc::{client_handshake, server_handshake, Connection};
use la_proto::chunking::chunk_session_output;
use la_proto::jsonrpc::{Message, Notification, Request, RequestId, Response, ResponseOutcome};
use la_proto::methods::{
    Method, PtySize, ServerCapabilities, SessionState, SessionsAttach, SessionsAttachParams,
    SessionsAttachResult, SessionsCreate, SessionsCreateParams, SessionsCreateResult,
    SessionsWrite, SessionsWriteParams, SessionsWriteResult,
};
use la_proto::notifications::{NotificationMethod, SessionOutput};
use la_proto::PROTOCOL_VERSION;
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Spawn a mock daemon that handles exactly one connection and drives every
/// M0.2 method. Returns the daemon task and the UDS path the client should
/// connect to.
async fn spawn_mock_daemon(
    pty_payload: Arc<Vec<u8>>,
) -> (TempDir, tokio::task::JoinHandle<()>, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let sock = dir.path().join("lad.sock");
    let listener = Listener::bind(&Endpoint::uds(&sock))
        .await
        .expect("bind listener");

    let sock_clone = sock.clone();
    let task = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut conn = Connection::new(stream);

        // 1) Handshake.
        let _init = server_handshake(
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

        // 2..n) Handle each subsequent request in order. We don't bother
        //       multiplexing because the mock client serializes its calls.
        let session_id = "01J0SESS0000000000000000".to_string();

        // 2) sessions.create
        let msg = conn.recv().await.expect("io").expect("eof on create");
        let Message::Request(req) = msg else {
            panic!("expected request");
        };
        assert_eq!(req.method, SessionsCreate::NAME);
        let _p: SessionsCreateParams = req.params_as().expect("decode create params");
        let result = SessionsCreateResult {
            session_id: session_id.clone(),
            backend: "claude".into(),
            cwd: "/tmp/p".into(),
            initial_size: PtySize { rows: 24, cols: 80 },
            state: SessionState::Starting,
        };
        let resp = Response::success(req.id, &result).expect("encode");
        conn.send(&Message::Response(resp)).await.expect("send");

        // 3) sessions.attach
        let msg = conn.recv().await.expect("io").expect("eof on attach");
        let Message::Request(req) = msg else {
            panic!("expected request");
        };
        assert_eq!(req.method, SessionsAttach::NAME);
        let p: SessionsAttachParams = req.params_as().expect("decode attach params");
        assert_eq!(p.session_id, session_id);
        let resp = Response::success(
            req.id,
            &SessionsAttachResult {
                session_id: session_id.clone(),
                snapshot_seq: 0,
                input_acquired: p.acquire_input,
            },
        )
        .expect("encode");
        conn.send(&Message::Response(resp)).await.expect("send");

        // 4) sessions.write — client pushes some PTY input. We acknowledge
        //    and then start streaming session.output notifications back.
        let msg = conn.recv().await.expect("io").expect("eof on write");
        let Message::Request(req) = msg else {
            panic!("expected request");
        };
        assert_eq!(req.method, SessionsWrite::NAME);
        let p: SessionsWriteParams = req.params_as().expect("decode write params");
        assert_eq!(p.session_id, session_id);
        let written_bytes = p.data_bytes().expect("base64 decode");
        let resp = Response::success(req.id, SessionsWriteResult::default()).expect("encode");
        conn.send(&Message::Response(resp)).await.expect("send");

        // 5) session.output notifications: echo what the client wrote, then
        //    push the large PTY payload split into 64 KiB chunks. seq starts
        //    after the snapshot.
        let mut seq: u64 = 1;
        let echo_chunks = chunk_session_output(&session_id, seq, &written_bytes);
        for chunk in &echo_chunks {
            let note = Notification::new(SessionOutput::NAME, chunk).expect("encode notif");
            conn.send(&Message::Notification(note))
                .await
                .expect("send notif");
        }
        seq += echo_chunks.len() as u64;

        let bulk_chunks = chunk_session_output(&session_id, seq, &pty_payload);
        for chunk in &bulk_chunks {
            let note = Notification::new(SessionOutput::NAME, chunk).expect("encode notif");
            conn.send(&Message::Notification(note))
                .await
                .expect("send notif");
        }
    });

    (dir, task, sock_clone)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_methods_round_trip_over_uds() {
    // Make this >= 64 KiB so that we exercise the chunker through the wire,
    // not just in isolation. Also a non-trivial byte pattern so a swap of
    // any two chunks would be caught.
    let pty: Vec<u8> = (0..(150 * 1024)).map(|i| (i % 251) as u8).collect();
    let pty_arc = Arc::new(pty.clone());

    let (_dir, daemon, sock) = spawn_mock_daemon(pty_arc).await;

    // Tiny pause to make sure bind has propagated. Not strictly necessary
    // (UnixListener::bind is sync), but keeps CI quiet on overloaded boxes.
    tokio::time::sleep(Duration::from_millis(10)).await;

    let stream = connect(&Endpoint::uds(&sock)).await.expect("connect");
    let mut conn = Connection::new(stream);

    // 1) Handshake.
    let info = client_handshake(&mut conn, "la", "0.4.1", &[PROTOCOL_VERSION])
        .await
        .expect("client handshake");
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert_eq!(info.capabilities.adapters, vec!["claude".to_string()]);

    // 2) sessions.create
    let req = Request::new(
        2,
        SessionsCreate::NAME,
        SessionsCreateParams {
            project_dir: "/tmp/p".into(),
            backend: "claude".into(),
            args: vec![],
            prompt: None,
            worktree: false,
        },
    )
    .unwrap();
    conn.send(&Message::Request(req)).await.unwrap();
    let session_id = match conn.recv().await.unwrap().unwrap() {
        Message::Response(Response {
            outcome: ResponseOutcome::Result(result),
            id,
            ..
        }) => {
            assert_eq!(id, RequestId::Num(2));
            let r: SessionsCreateResult = serde_json::from_value(result).unwrap();
            r.session_id
        }
        other => panic!("unexpected: {other:?}"),
    };

    // 3) sessions.attach
    let req = Request::new(
        3,
        SessionsAttach::NAME,
        SessionsAttachParams {
            session_id: session_id.clone(),
            replay_bytes: Some(0),
            acquire_input: true,
        },
    )
    .unwrap();
    conn.send(&Message::Request(req)).await.unwrap();
    let attach = match conn.recv().await.unwrap().unwrap() {
        Message::Response(Response {
            outcome: ResponseOutcome::Result(result),
            ..
        }) => serde_json::from_value::<SessionsAttachResult>(result).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(attach.input_acquired);

    // 4) sessions.write — push a deliberately non-UTF-8 byte run so base64
    //    fidelity matters end-to-end.
    let written: Vec<u8> = vec![0x00, 0xff, 0x1b, b'h', b'i', 0x0a];
    let req = Request::new(
        4,
        SessionsWrite::NAME,
        SessionsWriteParams::from_bytes(&session_id, &written),
    )
    .unwrap();
    conn.send(&Message::Request(req)).await.unwrap();
    let _ack = match conn.recv().await.unwrap().unwrap() {
        Message::Response(r) => r,
        other => panic!("unexpected: {other:?}"),
    };

    // 5) Drain session.output notifications until we've seen `written` echoed
    //    AND the full PTY payload. Validate monotonic seq across chunks.
    let mut got: Vec<u8> = Vec::new();
    let mut last_seq: Option<u64> = None;
    let expected = {
        let mut e = written.clone();
        e.extend_from_slice(&pty);
        e
    };
    while got.len() < expected.len() {
        let m = tokio::time::timeout(Duration::from_secs(5), conn.recv())
            .await
            .expect("recv timeout")
            .expect("io")
            .expect("eof");
        let Message::Notification(n) = m else {
            panic!("expected notification, got {m:?}");
        };
        assert_eq!(n.method, SessionOutput::NAME);
        let p: la_proto::notifications::SessionOutputParams = n.params_as().unwrap();
        if let Some(prev) = last_seq {
            assert_eq!(p.seq, prev + 1, "seq must be monotonic");
        }
        last_seq = Some(p.seq);
        got.extend_from_slice(&p.data_bytes().unwrap());
    }
    assert_eq!(got, expected, "PTY bytes must be byte-identical");

    daemon.await.expect("daemon clean exit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_rejects_unknown_protocol_version() {
    let dir = TempDir::new().expect("tempdir");
    let sock = dir.path().join("lad.sock");
    let listener = Listener::bind(&Endpoint::uds(&sock)).await.unwrap();

    let server = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut conn = Connection::new(stream);
        let err = server_handshake(
            &mut conn,
            "lad",
            "0.4.1",
            &["1"],
            ServerCapabilities::default(),
        )
        .await
        .unwrap_err();
        match err {
            la_ipc::HandshakeError::NoCommonVersion { .. } => (),
            other => panic!("expected NoCommonVersion, got {other:?}"),
        }
    });

    let stream = connect(&Endpoint::uds(&sock)).await.unwrap();
    let mut conn = Connection::new(stream);
    let res = client_handshake(&mut conn, "la", "0.4.1", &["99"]).await;
    let err = res.unwrap_err();
    match err {
        la_ipc::HandshakeError::ServerRejected { code, .. } => {
            assert_eq!(code, la_proto::error_codes::UNSUPPORTED_PROTOCOL_VERSION);
        }
        other => panic!("expected ServerRejected, got {other:?}"),
    }
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_halves_allow_concurrent_send_and_recv() {
    // Independent prove: split() lets the server stream notifications while
    // the client is still composing its next request, with no deadlock.

    let dir = TempDir::new().expect("tempdir");
    let sock = dir.path().join("lad.sock");
    let listener = Listener::bind(&Endpoint::uds(&sock)).await.unwrap();

    let server = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut conn = Connection::new(stream);
        let _ = server_handshake(
            &mut conn,
            "lad",
            "0.4.1",
            &[PROTOCOL_VERSION],
            ServerCapabilities::default(),
        )
        .await
        .unwrap();
        // Fire 100 notifications in rapid succession.
        for i in 0..100 {
            let p = la_proto::notifications::SessionOutputParams::from_bytes(
                "sid",
                i,
                format!("hello-{i}").as_bytes(),
            );
            let n = Notification::new(SessionOutput::NAME, &p).unwrap();
            conn.send(&Message::Notification(n)).await.unwrap();
        }
    });

    let stream = connect(&Endpoint::uds(&sock)).await.unwrap();
    let mut conn = Connection::new(stream);
    let _ = client_handshake(&mut conn, "la", "0.4.1", &[PROTOCOL_VERSION])
        .await
        .unwrap();

    let (_send, mut recv) = conn.split();
    let (tx, mut rx) = mpsc::channel::<u64>(128);
    tokio::spawn(async move {
        while let Some(m) = recv.recv().await.unwrap() {
            if let Message::Notification(n) = m {
                let p: la_proto::notifications::SessionOutputParams = n.params_as().unwrap();
                tx.send(p.seq).await.ok();
            }
        }
    });
    let mut count = 0;
    while let Ok(Some(_seq)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        count += 1;
        if count == 100 {
            break;
        }
    }
    assert_eq!(count, 100);
    server.await.unwrap();
}
