//! WEK-18 unit + integration coverage for the `la-core` session manager.
//!
//! Acceptance hooks from the issue body:
//!
//! - **Unit: spawn → state sequence `starting/running/(waiting)/exited`
//!   correct** → [`spawn_emits_starting_running_exited`] and
//!   [`spawn_emits_waiting_after_idle`].
//! - **Unit: client disconnect doesn't kill the child** (the §1.2 "la 永远
//!   不直接持有 PTY" invariant) → [`detach_does_not_kill_child`].
//! - **Integration with M1.2: 多 attach 一写多读** → [`multi_attach_one_writer`].
//!
//! Also exercises the orphan reaper and the writer-lock invariant so a
//! future refactor can't quietly regress §3.

mod support;

use std::time::Duration;

use la_core::CoreError;
use la_ipc::HubEvent;
use la_proto::methods::{SessionSignal, SessionState};
use la_storage::NewSession;

use support::{new_manager, request_in, ShellAdapter, TEST_BACKEND};

// Helper: drain the event bus into a Vec<SessionState> until a terminal
// state is observed or the timeout fires. Other event types are ignored.
async fn collect_states(
    mut rx: tokio::sync::broadcast::Receiver<la_core::BusEvent>,
    timeout: Duration,
    stop_on: impl Fn(SessionState) -> bool + Send + 'static,
) -> Vec<(SessionState, Option<i32>)> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(la_core::BusEvent::SessionState(p))) => {
                let state = p.state;
                out.push((state, p.exit_code));
                if stop_on(state) {
                    break;
                }
            }
            Ok(Ok(_other)) => continue,
            Ok(Err(_lagged_or_closed)) => break,
            Err(_) => break,
        }
    }
    out
}

#[tokio::test]
async fn spawn_emits_starting_running_exited() {
    let h = new_manager(false).await;
    let rx = h.manager.bus().subscribe();
    let adapter = ShellAdapter::new(
        // Print one line then exit fast — we want the running+exited path.
        "printf 'hello\\n'; exit 0",
    );

    let spawned = h
        .manager
        .spawn(&adapter, h.project_id.clone(), request_in(&h.project_root))
        .await
        .expect("spawn");
    assert_eq!(spawned.backend, TEST_BACKEND);
    assert_eq!(spawned.initial_state, SessionState::Starting);

    let states = collect_states(rx, Duration::from_secs(5), |s| {
        matches!(s, SessionState::Exited | SessionState::Errored)
    })
    .await;

    // Architecture §3 lifecycle: starting → running → exited (waiting is
    // optional and absent on a fast script).
    let just_states: Vec<SessionState> = states.iter().map(|(s, _)| *s).collect();
    assert!(
        just_states.contains(&SessionState::Starting),
        "missing Starting in {just_states:?}",
    );
    assert!(
        just_states.contains(&SessionState::Running),
        "missing Running in {just_states:?}",
    );
    assert!(
        matches!(just_states.last(), Some(SessionState::Exited)),
        "should end Exited, got {just_states:?}",
    );

    let (_, exit_code) = states
        .iter()
        .find(|(s, _)| *s == SessionState::Exited)
        .copied()
        .expect("Exited event");
    assert_eq!(exit_code, Some(0));

    // Storage row should reflect the terminal state once the pump persists.
    // The pump persists state before publishing, so by the time we got the
    // event the row is already updated.
    let row = h
        .storage
        .sessions()
        .get(spawned.id.as_str())
        .await
        .expect("get row")
        .expect("session row exists");
    assert_eq!(row.state, "exited");
    assert_eq!(row.exit_code, Some(0));
}

#[tokio::test]
async fn spawn_emits_waiting_after_idle() {
    let h = new_manager(false).await;
    let rx = h.manager.bus().subscribe();
    // Print, then sleep — by the time the 100 ms idle threshold expires
    // the session should be flipped to Waiting; then it exits.
    let adapter = ShellAdapter::new("printf 'x'; sleep 0.4; exit 0");

    let _spawned = h
        .manager
        .spawn(&adapter, h.project_id.clone(), request_in(&h.project_root))
        .await
        .expect("spawn");

    let states = collect_states(rx, Duration::from_secs(5), |s| s == SessionState::Exited).await;
    let just_states: Vec<SessionState> = states.iter().map(|(s, _)| *s).collect();
    assert!(
        just_states.contains(&SessionState::Waiting),
        "expected Waiting between Running and Exited, got {just_states:?}",
    );
}

#[tokio::test]
async fn detach_does_not_kill_child() {
    // §1.2 关键不变量: "la 永远不直接持有 PTY" — closing a client must NOT
    // affect the child. We emulate that by attaching, detaching, and
    // confirming the session still runs to completion + publishes Exited.
    let h = new_manager(false).await;
    let rx = h.manager.bus().subscribe();
    // The child writes one byte after 150 ms — long enough that we can
    // detach before it finishes, which proves the detach didn't kill it.
    let adapter = ShellAdapter::new("sleep 0.15; printf 'y'; exit 0");

    let spawned = h
        .manager
        .spawn(&adapter, h.project_id.clone(), request_in(&h.project_root))
        .await
        .expect("spawn");

    let attach = h
        .manager
        .attach(&spawned.id, None, true)
        .await
        .expect("attach");
    assert!(attach.input_acquired, "first attach should hold input");
    // Drop the subscription via detach (which parks + schedules eviction).
    let sub_id = attach.subscription.id();
    drop(attach.subscription);
    h.manager.detach(&spawned.id, sub_id).await.expect("detach");

    // Now we should still see Exited on the bus — the child kept running.
    let states = collect_states(rx, Duration::from_secs(5), |s| s == SessionState::Exited).await;
    let just_states: Vec<SessionState> = states.iter().map(|(s, _)| *s).collect();
    assert!(
        matches!(just_states.last(), Some(SessionState::Exited)),
        "detach must not kill child; states={just_states:?}",
    );
}

#[tokio::test]
async fn writer_lock_refuses_second_writer() {
    let h = new_manager(false).await;
    // Long-lived child so we have time to test the writer-lock policy.
    let adapter = ShellAdapter::new("sleep 0.5; exit 0");
    let spawned = h
        .manager
        .spawn(&adapter, h.project_id.clone(), request_in(&h.project_root))
        .await
        .expect("spawn");

    let writer_attach = h
        .manager
        .attach(&spawned.id, None, true)
        .await
        .expect("attach writer");
    assert!(writer_attach.input_acquired);

    // Second attach with acquire_input=true: succeeds as read-only.
    let reader_attach = h
        .manager
        .attach(&spawned.id, None, true)
        .await
        .expect("attach reader");
    assert!(
        !reader_attach.input_acquired,
        "second writer should be denied per §3",
    );

    // Reader trying to write must get WriterLocked, not NotAttached.
    let err = h
        .manager
        .write(&spawned.id, reader_attach.subscription.id(), b"x")
        .await
        .expect_err("read-only sub can't write");
    assert!(matches!(err, CoreError::WriterLocked { .. }), "got {err:?}");

    // Holder writing succeeds.
    h.manager
        .write(&spawned.id, writer_attach.subscription.id(), b"\n")
        .await
        .expect("writer can write");
}

#[tokio::test]
async fn multi_attach_one_writer_one_reader_share_output() {
    // M1.4 ↔ M1.2 integration: confirm two subscriptions on the same
    // session both see the same `session.output` chunks the hub fans out,
    // and that the writer-only sub can also write. This exercises the
    // `OutputHub::publish → Subscription::recv` path end-to-end through
    // the manager façade.
    let h = new_manager(false).await;
    // Slow-drip script so the test can attach before the child finishes.
    let adapter = ShellAdapter::new("printf 'a'; sleep 0.05; printf 'b'; exit 0");

    let spawned = h
        .manager
        .spawn(&adapter, h.project_id.clone(), request_in(&h.project_root))
        .await
        .expect("spawn");

    let mut writer = h
        .manager
        .attach(&spawned.id, None, true)
        .await
        .expect("attach writer");
    assert!(writer.input_acquired);

    let mut reader = h
        .manager
        .attach(&spawned.id, None, false)
        .await
        .expect("attach reader");
    assert!(!reader.input_acquired);

    let collect = |mut sub: la_ipc::Subscription| async move {
        let mut bytes = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break bytes;
            }
            match tokio::time::timeout(remaining, sub.recv()).await {
                Ok(Some(HubEvent::Output(p))) => {
                    bytes.extend(p.data_bytes().expect("decode"));
                    if bytes.contains(&b'b') {
                        break bytes;
                    }
                }
                Ok(Some(_other)) => continue,
                Ok(None) => break bytes,
                Err(_) => break bytes,
            }
        }
    };

    // Move subs into tasks so they race against the child producing bytes.
    let writer_sub = std::mem::replace(&mut writer.subscription, dummy_sub(&h, &spawned.id).await);
    let reader_sub = std::mem::replace(&mut reader.subscription, dummy_sub(&h, &spawned.id).await);
    let (w_bytes, r_bytes) = tokio::join!(collect(writer_sub), collect(reader_sub));
    assert!(w_bytes.contains(&b'a'), "writer missed 'a': {w_bytes:?}");
    assert!(w_bytes.contains(&b'b'), "writer missed 'b': {w_bytes:?}");
    assert!(r_bytes.contains(&b'a'), "reader missed 'a': {r_bytes:?}");
    assert!(r_bytes.contains(&b'b'), "reader missed 'b': {r_bytes:?}");
}

// Helper used by multi_attach_one_writer_… to swap a Subscription out of
// the AttachOutcome (so we can move it into a task) without needing a
// public Default impl on Subscription. The "dummy" replacement is just
// another live attach — we drop it immediately by letting it go out of
// scope at the end of the test.
async fn dummy_sub(h: &support::TestHarness, id: &la_core::SessionId) -> la_ipc::Subscription {
    // Detached extra attach; we don't use it but it satisfies the type.
    h.manager
        .attach(id, None, false)
        .await
        .expect("dummy attach")
        .subscription
}

#[tokio::test]
async fn delete_refuses_running_session() {
    let h = new_manager(false).await;
    let adapter = ShellAdapter::new("sleep 0.5; exit 0");
    let spawned = h
        .manager
        .spawn(&adapter, h.project_id.clone(), request_in(&h.project_root))
        .await
        .expect("spawn");
    // Immediate delete should be refused (registry still has the session).
    let err = h.manager.delete(&spawned.id).await.expect_err("refused");
    assert!(matches!(err, CoreError::SessionBusy), "got {err:?}");
}

#[tokio::test]
async fn orphan_reaper_marks_stale_rows_exited() {
    // §6.3: storage rows in starting/running with no live pid should be
    // marked exited{unknown} on daemon startup.
    let h = new_manager(false).await;
    let id = la_storage::new_id();
    h.storage
        .sessions()
        .create(NewSession {
            id: id.clone(),
            project_id: h.project_id.clone(),
            backend_id: TEST_BACKEND.to_string(),
            external_id: None,
            title: None,
            state: "running".to_string(),
            pid: Some(1), // pid 1 exists but isn't OUR child — reaper still treats it as alive
            worktree_path: None,
            worktree_branch: None,
            base_branch: None,
            spawn_args: serde_json::json!({}),
            origin: "user".to_string(),
            post_create_hook_status: None,
        })
        .await
        .expect("insert orphan");
    // Now overwrite pid with a clearly-dead one (max u32) so the reaper
    // can do its job.
    h.storage
        .sessions()
        .update_pid(&id, Some(i64::from(u32::MAX) - 1))
        .await
        .expect("set fake pid");

    let reaped = h.manager.reap_orphans().await.expect("reap");
    assert!(reaped >= 1, "expected at least 1 row reaped");

    let row = h
        .storage
        .sessions()
        .get(&id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.state, "exited");
}

#[tokio::test]
async fn signal_int_terminates_long_running_session() {
    let h = new_manager(false).await;
    let rx = h.manager.bus().subscribe();
    // Bash that traps INT politely and exits 130 (the conventional code).
    let adapter = ShellAdapter::new("trap 'exit 130' INT; sleep 5");

    let spawned = h
        .manager
        .spawn(&adapter, h.project_id.clone(), request_in(&h.project_root))
        .await
        .expect("spawn");
    // Give the child a moment to install the trap before we signal it.
    tokio::time::sleep(Duration::from_millis(150)).await;
    h.manager
        .signal(&spawned.id, SessionSignal::Int)
        .await
        .expect("signal");

    let states = collect_states(rx, Duration::from_secs(3), |s| s == SessionState::Exited).await;
    let exit = states
        .iter()
        .find(|(s, _)| *s == SessionState::Exited)
        .expect("Exited");
    // sh may exit 130 OR 0 depending on whether the trap fired before
    // sleep had a chance — we just care that it exited promptly, not
    // that the script was still sleeping at 5 s.
    assert!(exit.1.is_some(), "exit code should be present");
}
