// Uses `la_ipc::transport::Listener::bind` with a UDS endpoint, which
// returns `Unsupported "UDS is not available on Windows"` at runtime.
// Gating the file to unix keeps WEK-72's matrix CI green on
// windows-2022; the Windows transport has its own coverage.
#![cfg(unix)]

//! WEK-93 / A1 smoke test: [`RpcSessionSource`] against a fake `lad`.
//!
//! Exercises the contract the binary swap depends on:
//!
//! 1. After connect + handshake, the first `sessions.list` round-trip
//!    populates the cache so [`RpcSessionSource::snapshot`] returns a
//!    non-empty grouping reflecting the stub's payload.
//! 2. `archive(session_id)` actually emits a `sessions.archive` RPC
//!    on the wire AND triggers an immediate `sessions.list` refresh,
//!    so the next snapshot reflects the daemon's post-archive view.
//! 3. When the daemon is offline (no listener bound), `snapshot()`
//!    returns an empty Vec rather than panicking. This is the
//!    "daemon 离线时给出友好错误状态，不 panic" acceptance line.

use std::sync::Arc;
use std::time::{Duration, Instant};

use la_ipc::transport::{Endpoint, Listener};
use la_ipc::{server_handshake, Connection};
use la_proto::jsonrpc::{Message, Response};
use la_proto::methods::{
    Method, PtySize, ServerCapabilities, SessionState, SessionSummary, SessionsArchive,
    SessionsArchiveResult, SessionsCreate, SessionsCreateParams, SessionsCreateResult,
    SessionsList, SessionsListResult,
};
use la_tui::source::{NewSessionRequest, RpcSessionSource, SourceError};
use la_tui::SessionSource;
use tokio::sync::Mutex;

fn make_summary(id: &str, project: &str, state: SessionState) -> SessionSummary {
    SessionSummary {
        session_id: id.into(),
        project_id: project.into(),
        backend: "claude".into(),
        title: None,
        state,
        origin: "user".into(),
        created_at: "2026-06-06T00:00:00Z".into(),
        updated_at: "2026-06-06T00:00:00Z".into(),
        worktree_path: Some(format!("/tmp/{project}")),
    }
}

async fn handshake_stub(conn: &mut Connection<tokio::net::UnixStream>) {
    let caps = ServerCapabilities {
        adapters: vec!["claude".into()],
        cron: true,
        worktree: false,
        diff: false,
        events: true,
    };
    server_handshake(
        conn,
        "lad-stub",
        "0.0.0",
        &[la_proto::PROTOCOL_VERSION],
        caps,
    )
    .await
    .expect("handshake");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_reflects_sessions_list_after_handshake() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-rpc-src-1.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");

    let server = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut conn = Connection::new(stream);
        handshake_stub(&mut conn).await;

        // Reply to one sessions.list, then park until the test thread
        // drops its sender (causing the source's bg loop to exit and
        // close this connection).
        let msg = conn.recv().await.expect("recv").expect("eof");
        if let Message::Request(req) = msg {
            assert_eq!(req.method, SessionsList::NAME);
            let result = SessionsListResult {
                sessions: vec![
                    make_summary("s1", "proj-a", SessionState::Running),
                    make_summary("s2", "proj-a", SessionState::Exited),
                    make_summary("s3", "proj-b", SessionState::Archived),
                ],
            };
            let resp = Response::success(req.id, &result).expect("encode list result");
            conn.send(&Message::Response(resp)).await.expect("send ack");
        } else {
            panic!("expected sessions.list request");
        }
        // Idle until disconnect.
        let _ = conn.recv().await;
    });

    let source = RpcSessionSource::connect(&socket);
    let populated = tokio::task::spawn_blocking(move || {
        let ok = source.wait_for_first_snapshot(Duration::from_secs(5));
        (ok, source.snapshot())
    })
    .await
    .expect("blocking join");

    let (ok, groups) = populated;
    assert!(ok, "cache never filled within deadline");
    // Expect proj-a (with 2 sessions) and the archived bucket
    // containing s3.
    let proj_a = groups
        .iter()
        .find(|g| g.project_id == "proj-a")
        .expect("proj-a group missing");
    assert_eq!(proj_a.sessions.len(), 2);
    assert_eq!(proj_a.display_name, "proj-a");
    assert_eq!(proj_a.root_path, "/tmp/proj-a");
    let archived = groups.last().unwrap();
    assert!(archived.is_archived, "archived bucket pinned last");
    assert_eq!(archived.sessions.len(), 1);
    assert_eq!(archived.sessions[0].session_id, "s3");

    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_emits_rpc_and_triggers_immediate_refresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-rpc-src-2.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");

    let archive_seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let archive_seen_bg = archive_seen.clone();
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut conn = Connection::new(stream);
        handshake_stub(&mut conn).await;

        // Track which session ids have been archived so subsequent
        // sessions.list responses can drop them from the "active"
        // bucket. This mirrors how the real daemon would behave.
        loop {
            let msg = match conn.recv().await {
                Ok(Some(m)) => m,
                _ => return,
            };
            let req = match msg {
                Message::Request(r) => r,
                _ => continue,
            };
            match req.method.as_str() {
                m if m == SessionsList::NAME => {
                    let archived = archive_seen_bg.lock().await.clone();
                    let mut sessions = vec![
                        make_summary("s1", "proj-a", SessionState::Running),
                        make_summary("s2", "proj-a", SessionState::Running),
                    ];
                    // If s1 has been archived, flip its state to Archived
                    // so it lands in the archive bucket on the TUI side.
                    for s in &mut sessions {
                        if archived.iter().any(|id| id == &s.session_id) {
                            s.state = SessionState::Archived;
                        }
                    }
                    let resp = Response::success(req.id, &SessionsListResult { sessions })
                        .expect("encode list");
                    conn.send(&Message::Response(resp)).await.expect("send");
                }
                m if m == SessionsArchive::NAME => {
                    let params: la_proto::methods::SessionsArchiveParams =
                        serde_json::from_value(req.params.expect("archive params"))
                            .expect("decode archive params");
                    archive_seen_bg.lock().await.push(params.session_id);
                    let resp = Response::success(req.id, &SessionsArchiveResult {})
                        .expect("encode archive ack");
                    conn.send(&Message::Response(resp)).await.expect("send");
                }
                other => {
                    panic!("unexpected method on stub: {other}");
                }
            }
        }
    });

    let mut source = RpcSessionSource::connect(&socket);
    let result = tokio::task::spawn_blocking(move || {
        assert!(
            source.wait_for_first_snapshot(Duration::from_secs(5)),
            "first snapshot never landed"
        );
        // Pre-archive: s1 is under proj-a, no archived bucket.
        let pre = source.snapshot();
        let pre_proj_a = pre
            .iter()
            .find(|g| g.project_id == "proj-a")
            .expect("proj-a present");
        assert_eq!(pre_proj_a.sessions.len(), 2);
        assert!(pre.iter().all(|g| !g.is_archived));

        source.archive("s1");

        // Wait for the cache to reflect the archive — bg loop sends
        // sessions.archive and then re-pulls sessions.list.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let snap = source.snapshot();
            if let Some(arch) = snap.iter().find(|g| g.is_archived) {
                if arch.sessions.iter().any(|s| s.session_id == "s1") {
                    return snap;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("archive never propagated to snapshot");
    })
    .await
    .expect("blocking join");

    let archived = result.iter().find(|g| g.is_archived).unwrap();
    assert_eq!(archived.sessions.len(), 1);
    assert_eq!(archived.sessions[0].session_id, "s1");
    let proj_a = result
        .iter()
        .find(|g| g.project_id == "proj-a")
        .expect("proj-a still present");
    assert_eq!(proj_a.sessions.len(), 1);
    assert_eq!(proj_a.sessions[0].session_id, "s2");

    let seen = archive_seen.lock().await.clone();
    assert_eq!(seen, vec!["s1".to_string()]);

    // Drop the server task / socket.
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_returns_empty_when_daemon_is_offline() {
    // No listener bound — connect will fail. The bg loop should retry
    // with backoff; snapshot() must not panic, just return an empty
    // Vec for the sidebar to render as "no projects yet".
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-rpc-src-3.sock");
    let source = RpcSessionSource::connect(&socket);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let snap = tokio::task::spawn_blocking(move || source.snapshot())
        .await
        .expect("blocking join");
    assert!(
        snap.is_empty(),
        "expected empty snapshot for offline daemon, got {snap:?}"
    );
    drop(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_times_out_when_daemon_never_replies() {
    // WEK-94 / PR #86 review: a wedged daemon (socket open, sessions.list
    // works, sessions.create stalls) must NOT freeze the TUI. The
    // RpcSessionSource::create_session call has to give up after a
    // bounded wait so the modal can close with a Backend toast and the
    // user can hit `Esc`. This test stands up a stub that completes
    // the initial sessions.list (so the source is "healthy") but then
    // silently drops the sessions.create request, and asserts the
    // synchronous trait call returns Err(SourceError::Backend(...))
    // referencing "timed out" within the override window.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-rpc-src-4.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");

    let server = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut conn = Connection::new(stream);
        handshake_stub(&mut conn).await;
        loop {
            let msg = match conn.recv().await {
                Ok(Some(m)) => m,
                _ => return,
            };
            let req = match msg {
                Message::Request(r) => r,
                _ => continue,
            };
            match req.method.as_str() {
                m if m == SessionsList::NAME => {
                    let resp = Response::success(
                        req.id,
                        &SessionsListResult {
                            sessions: vec![make_summary("s1", "proj-a", SessionState::Running)],
                        },
                    )
                    .expect("encode list");
                    conn.send(&Message::Response(resp))
                        .await
                        .expect("send list");
                }
                m if m == SessionsCreate::NAME => {
                    // Intentionally drop the request — never reply.
                    // The TUI thread should time out, not wedge.
                }
                _ => {}
            }
        }
    });

    let mut source = RpcSessionSource::connect(&socket);
    // 800 ms override is long enough to be insensitive to scheduler
    // jitter on loaded CI, short enough to keep the test well under
    // 1 s of wall-clock.
    source.set_create_timeout(Duration::from_millis(800));
    let result = tokio::task::spawn_blocking(move || {
        // Make sure the initial sessions.list landed so we know the
        // bg loop is healthy and the timeout is exercising the create
        // path, not a stuck handshake.
        assert!(
            source.wait_for_first_snapshot(Duration::from_secs(5)),
            "first sessions.list never landed"
        );
        let started = Instant::now();
        let res = source.create_session(NewSessionRequest {
            project_dir: "/tmp/proj-a".into(),
            backend: "claude".into(),
            args: Vec::new(),
            worktree: false,
        });
        (res, started.elapsed())
    })
    .await
    .expect("blocking join");

    let (res, elapsed) = result;
    let err = res.expect_err("create_session must surface a Backend error on timeout");
    match err {
        SourceError::Backend(msg) => {
            assert!(
                msg.contains("timed out"),
                "expected timeout reason in Backend error, got {msg:?}"
            );
        }
        other => panic!("expected SourceError::Backend, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(3),
        "create_session returned in {elapsed:?}, expected to honour the override"
    );

    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

// WEK-92-A4.1: read-your-write — `create_session` MUST return only
// after a follow-up `sessions.list` has rebuilt the cache to include
// the new row. Before the fix the bg loop acked the caller first and
// re-pulled on the next loop turn, so the TUI's immediate
// `refresh_sessions()` saw the pre-create cache and the sidebar
// missed the new row for at least one frame (worst case: until the
// next 2 s poll tick + a user keystroke). The stub here serves a
// pre-create `sessions.list` of `[s1]` and a post-create one of
// `[s1, mock-1]`, and asserts the snapshot the caller reads
// immediately after the trait call returns already contains `mock-1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_session_pulls_fresh_snapshot_before_acking_caller() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-rpc-src-5.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");

    let created = Arc::new(Mutex::new(Vec::<String>::new()));
    let created_bg = created.clone();
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut conn = Connection::new(stream);
        handshake_stub(&mut conn).await;
        loop {
            let msg = match conn.recv().await {
                Ok(Some(m)) => m,
                _ => return,
            };
            let req = match msg {
                Message::Request(r) => r,
                _ => continue,
            };
            match req.method.as_str() {
                m if m == SessionsList::NAME => {
                    let mut sessions = vec![make_summary("s1", "proj-a", SessionState::Running)];
                    // Once a create has been ack'd, the list response
                    // grows to include the new row. The race we are
                    // pinning down is "did the bg loop call this method
                    // BEFORE telling the TUI thread the create
                    // succeeded?".
                    for id in created_bg.lock().await.iter() {
                        sessions.push(make_summary(id, "proj-a", SessionState::Running));
                    }
                    let resp = Response::success(req.id, &SessionsListResult { sessions })
                        .expect("encode list");
                    conn.send(&Message::Response(resp)).await.expect("send");
                }
                m if m == SessionsCreate::NAME => {
                    let params: SessionsCreateParams =
                        serde_json::from_value(req.params.expect("create params"))
                            .expect("decode create params");
                    assert_eq!(params.backend, "claude");
                    let new_id = "mock-1".to_string();
                    created_bg.lock().await.push(new_id.clone());
                    let resp = Response::success(
                        req.id,
                        &SessionsCreateResult {
                            session_id: new_id,
                            backend: "claude".into(),
                            cwd: "/tmp/proj-a".into(),
                            initial_size: PtySize { rows: 24, cols: 80 },
                            state: SessionState::Running,
                        },
                    )
                    .expect("encode create");
                    conn.send(&Message::Response(resp)).await.expect("send");
                }
                other => panic!("unexpected method on stub: {other}"),
            }
        }
    });

    let mut source = RpcSessionSource::connect(&socket);
    let snap = tokio::task::spawn_blocking(move || {
        assert!(
            source.wait_for_first_snapshot(Duration::from_secs(5)),
            "first sessions.list never landed"
        );
        let gen_before = source.refresh_generation();
        let id = source
            .create_session(NewSessionRequest {
                project_dir: "/tmp/proj-a".into(),
                backend: "claude".into(),
                args: Vec::new(),
                worktree: false,
            })
            .expect("create ok");
        assert_eq!(id.as_str(), "mock-1");
        // The cache MUST already include mock-1 — no sleep, no retry.
        // This is the contract the App's modal Confirm path relies on:
        // create_session resolves only after the snapshot has been
        // refreshed to the post-create view.
        let snap = source.snapshot();
        // refresh_generation must also reflect that a refresh has
        // happened between gen_before and now, so the runner has a
        // signal to dispatch RefreshSessions.
        assert!(
            source.refresh_generation() > gen_before,
            "refresh_generation must bump on the create's follow-up refresh: \
             before={gen_before}, after={}",
            source.refresh_generation()
        );
        snap
    })
    .await
    .expect("blocking join");

    let proj_a = snap
        .iter()
        .find(|g| g.project_id == "proj-a")
        .expect("proj-a present");
    let ids: Vec<&str> = proj_a
        .sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert!(
        ids.contains(&"mock-1"),
        "snapshot must include mock-1 immediately after create_session returns; got {ids:?}"
    );

    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}

// WEK-92-A4.1: the bg loop's refresh path bumps
// `refresh_generation()` so the runner can dispatch
// `AppMsg::RefreshSessions` without waiting for a keystroke. This is
// the contract the runner's per-frame check depends on; if the
// counter never moves the sidebar permanently displays whatever was
// true at startup. We serve two distinct `sessions.list` payloads
// across two ticks (forced via the test-only Refresh command so the
// test doesn't have to wait the full POLL_INTERVAL on every CI run)
// and assert both the cache and the counter shifted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_generation_bumps_after_bg_poll_tick() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lad-rpc-src-6.sock");
    let listener = Listener::bind(&Endpoint::uds(&socket))
        .await
        .expect("bind stub");

    let list_count = Arc::new(Mutex::new(0u32));
    let list_count_bg = list_count.clone();
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        let mut conn = Connection::new(stream);
        handshake_stub(&mut conn).await;
        loop {
            let msg = match conn.recv().await {
                Ok(Some(m)) => m,
                _ => return,
            };
            let req = match msg {
                Message::Request(r) => r,
                _ => continue,
            };
            match req.method.as_str() {
                m if m == SessionsList::NAME => {
                    let mut n = list_count_bg.lock().await;
                    *n += 1;
                    let sessions = if *n == 1 {
                        vec![make_summary("s1", "proj-a", SessionState::Running)]
                    } else {
                        // Second and subsequent ticks: add s2 so the
                        // grouped snapshot grows and the test can also
                        // verify the runner-visible cache changed (not
                        // just the counter).
                        vec![
                            make_summary("s1", "proj-a", SessionState::Running),
                            make_summary("s2", "proj-a", SessionState::Running),
                        ]
                    };
                    let resp = Response::success(req.id, &SessionsListResult { sessions })
                        .expect("encode list");
                    conn.send(&Message::Response(resp)).await.expect("send");
                }
                other => panic!("unexpected method on stub: {other}"),
            }
        }
    });

    let source = RpcSessionSource::connect(&socket);
    tokio::task::spawn_blocking(move || {
        assert!(
            source.wait_for_first_snapshot(Duration::from_secs(5)),
            "first sessions.list never landed"
        );
        let gen_after_first = source.refresh_generation();
        assert!(gen_after_first >= 1, "first refresh must bump the counter");
        // Force the second tick via the test-only Refresh command so
        // we don't have to wait the full POLL_INTERVAL on every CI run.
        // The contract is the same: any successful refresh bumps the
        // counter; the periodic poll is just one of the triggers.
        source.force_refresh();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let gen_now = source.refresh_generation();
            let snap = source.snapshot();
            if gen_now > gen_after_first {
                let proj_a = snap
                    .iter()
                    .find(|g| g.project_id == "proj-a")
                    .expect("proj-a present");
                if proj_a.sessions.len() == 2 {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let gen_now = source.refresh_generation();
        let snap = source.snapshot();
        panic!(
            "refresh_generation never moved past {gen_after_first}: now={gen_now}, snap={snap:?}"
        );
    })
    .await
    .expect("blocking join");

    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket);
    drop(dir);
}
