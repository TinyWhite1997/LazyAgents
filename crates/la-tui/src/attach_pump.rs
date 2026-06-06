//! Per-session attach pump.
//!
//! Owns a single daemon connection dedicated to one `sessions.attach`
//! subscription: calls `sessions.attach { acquire_input: true }`, decodes
//! `session.output` notifications into byte chunks the App can feed into a
//! [`crate::vte_term::TerminalScreen`], and forwards user keystrokes back
//! via `sessions.write`.
//!
//! Modelled on [`crate::notif_sub`]: a dedicated OS thread hosts a
//! current-thread tokio runtime and the daemon connection; events flow
//! out via an [`std::sync::mpsc`] channel; keystrokes flow in via a
//! second mpsc channel. The runner drains the outbound channel between
//! frames and pushes any pending writes into the daemon over the same
//! connection.
//!
//! The pump tracks the last `session.output.seq` it observed so a
//! reconnect (one and only one auto-retry) calls `sessions.attach` with
//! `resume_from_seq = Some(last_seq)`, exercising the
//! `reattach_with_resume_from_seq_catches_up_without_double_delivery`
//! contract from WEK-49.
//!
//! Lifetimes:
//!
//! * [`AttachPump::spawn`] hands back the pump handle (`stop()` to
//!   detach + tear down, and the inbound/outbound channels).
//! * Dropping the pump signals the thread to exit. Inflight bytes after
//!   that are discarded.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use la_ipc::transport::{connect, endpoint_for};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request, RequestId};
use la_proto::methods::{
    Method, SessionsAttach, SessionsAttachParams, SessionsAttachResult, SessionsDetach,
    SessionsDetachParams, SessionsWrite, SessionsWriteParams,
};
use la_proto::notifications::{
    NotificationMethod, SessionGap, SessionGapParams, SessionOutput, SessionOutputParams,
    SessionStateNotice, SessionStateParams,
};

/// Outbound events the pump emits up the App channel.
#[derive(Debug, Clone)]
pub enum AttachEvent {
    /// The `sessions.attach` round-trip succeeded; live stream is open.
    /// `snapshot_seq` is the last `seq` covered by the catch-up replay.
    Connected {
        session_id: String,
        snapshot_seq: u64,
        input_acquired: bool,
    },
    /// One `session.output` chunk. Bytes are the decoded PTY increment.
    Bytes { session_id: String, bytes: Vec<u8> },
    /// A `session.gap` notification — daemon evicted bytes between
    /// `from_seq` and `to_seq` before we could drain them.
    Gap {
        session_id: String,
        from_seq: u64,
        to_seq: u64,
        dropped_bytes: u64,
    },
    /// A `session.state` lifecycle transition (running / exited / errored).
    State {
        session_id: String,
        state: String,
        reason: Option<String>,
    },
    /// The pump lost its connection. `reason` is a short human label;
    /// `will_reconnect` is true on the first failure (an auto-retry is
    /// queued), false on the second consecutive failure (the user has to
    /// detach + re-enter to try again, matching the brief's "断线时显式
    /// 提示且自动尝试一次").
    Disconnected {
        reason: String,
        will_reconnect: bool,
    },
    /// The pump permanently stopped (user-driven detach or a second
    /// reconnect failure). No more events will arrive.
    Closed,
}

/// Inbound commands the runner sends to the pump.
#[derive(Debug, Clone)]
pub enum AttachCommand {
    /// User typed bytes that should hit the PTY master via `sessions.write`.
    Write(Vec<u8>),
    /// User asked to leave the attach. The pump emits a best-effort
    /// `sessions.detach` then closes.
    Detach,
}

/// Handle the App holds while an attach is live.
pub struct AttachPump {
    pub session_id: String,
    pub rx: Receiver<AttachEvent>,
    pub tx: Sender<AttachCommand>,
}

impl AttachPump {
    /// Spawn a new attach pump for `session_id`. The pump opens its own
    /// daemon connection and runs to completion on a dedicated OS thread.
    pub fn spawn(socket: &Path, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let socket_buf = socket.to_path_buf();
        let sid_for_thread = session_id.clone();
        std::thread::Builder::new()
            .name(format!("la-attach-{}", short_id(&session_id)))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        let _ = ev_tx.send(AttachEvent::Disconnected {
                            reason: format!("tokio runtime: {err}"),
                            will_reconnect: false,
                        });
                        let _ = ev_tx.send(AttachEvent::Closed);
                        return;
                    }
                };
                rt.block_on(driver(socket_buf, sid_for_thread, ev_tx, cmd_rx));
            })
            .expect("spawn la-attach thread");
        AttachPump {
            session_id,
            rx: ev_rx,
            tx: cmd_tx,
        }
    }

    /// Best-effort detach: enqueue the command and let the pump shut down.
    pub fn stop(&self) {
        let _ = self.tx.send(AttachCommand::Detach);
    }

    /// Forward a chunk of bytes to the daemon.
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(AttachCommand::Write(bytes));
    }
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

async fn driver(
    socket: PathBuf,
    session_id: String,
    ev_tx: Sender<AttachEvent>,
    cmd_rx: Receiver<AttachCommand>,
) {
    let mut last_seq: Option<u64> = None;
    let mut attempts: u32 = 0;
    // We allow exactly one auto-retry: the first connect failure raises
    // `will_reconnect = true`, the second raises `will_reconnect = false`
    // and exits. Per the brief: "断线时显式提示且自动尝试一次".
    loop {
        attempts += 1;
        let connected_now = run_one(
            &socket,
            &session_id,
            last_seq,
            &ev_tx,
            &cmd_rx,
            &mut last_seq,
        )
        .await;
        match connected_now {
            Ok(DriverExit::UserDetach) => {
                let _ = ev_tx.send(AttachEvent::Closed);
                return;
            }
            Ok(DriverExit::ChannelGone) => {
                // Runner shut down — quietly stop.
                return;
            }
            Err(err) => {
                let will_reconnect = attempts == 1;
                let _ = ev_tx.send(AttachEvent::Disconnected {
                    reason: err,
                    will_reconnect,
                });
                if !will_reconnect {
                    let _ = ev_tx.send(AttachEvent::Closed);
                    return;
                }
                // Brief backoff before the single retry so a flapping
                // daemon doesn't pin a CPU core.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

enum DriverExit {
    /// Runner asked to detach.
    UserDetach,
    /// Outbound or inbound channel went away — runner shut down.
    ChannelGone,
}

async fn run_one(
    socket: &Path,
    session_id: &str,
    resume_from: Option<u64>,
    ev_tx: &Sender<AttachEvent>,
    cmd_rx: &Receiver<AttachCommand>,
    last_seq: &mut Option<u64>,
) -> Result<DriverExit, String> {
    let endpoint = endpoint_for(socket);
    let stream = tokio::time::timeout(Duration::from_secs(2), connect(&endpoint))
        .await
        .map_err(|_| format!("timed out connecting to {}", socket.display()))?
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let mut conn = Connection::new(stream);
    let _info = client_handshake(
        &mut conn,
        "la-attach",
        env!("CARGO_PKG_VERSION"),
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .map_err(|e| format!("handshake: {e}"))?;

    // Issue sessions.attach. Pass the resume cursor so a reconnect
    // continues where we left off instead of double-delivering bytes
    // (WEK-49 contract: `reattach_with_resume_from_seq_catches_up_without_double_delivery`).
    let attach_id: i64 = 1;
    let attach_params = SessionsAttachParams {
        session_id: session_id.to_string(),
        resume_from_seq: resume_from,
        replay_bytes: None,
        acquire_input: true,
    };
    let attach_req = Request::new(attach_id, SessionsAttach::NAME, &attach_params)
        .map_err(|e| format!("encode sessions.attach: {e}"))?;
    conn.send(&Message::Request(attach_req))
        .await
        .map_err(|e| format!("send sessions.attach: {e}"))?;

    let attach_result = wait_for_response(&mut conn, RequestId::Num(attach_id)).await?;
    let attach: SessionsAttachResult = serde_json::from_value(attach_result)
        .map_err(|e| format!("decode SessionsAttachResult: {e}"))?;
    if ev_tx
        .send(AttachEvent::Connected {
            session_id: session_id.to_string(),
            snapshot_seq: attach.snapshot_seq,
            input_acquired: attach.input_acquired,
        })
        .is_err()
    {
        return Ok(DriverExit::ChannelGone);
    }

    // After a successful attach, treat the snapshot boundary as the
    // floor for `last_seq` so a subsequent reconnect picks up from
    // there even if no `session.output` lands between attach and drop.
    if last_seq.unwrap_or(0) < attach.snapshot_seq {
        *last_seq = Some(attach.snapshot_seq);
    }

    let mut next_req_id: i64 = attach_id + 1;
    loop {
        // Drain any pending outbound commands before blocking on the
        // socket so user keystrokes have one-frame latency, not poll-
        // interval latency.
        loop {
            match cmd_rx.try_recv() {
                Ok(AttachCommand::Write(bytes)) => {
                    let params =
                        match SessionsWriteParams::try_from_bytes(session_id.to_string(), &bytes) {
                            Ok(p) => p,
                            Err(err) => {
                                // Oversize writes are a programming error,
                                // not a transport failure — surface and drop.
                                tracing::warn!(%err, "attach-pump: dropping oversize write");
                                continue;
                            }
                        };
                    let id = next_req_id;
                    next_req_id += 1;
                    let req = Request::new(id, SessionsWrite::NAME, &params)
                        .map_err(|e| format!("encode sessions.write: {e}"))?;
                    conn.send(&Message::Request(req))
                        .await
                        .map_err(|e| format!("send sessions.write: {e}"))?;
                }
                Ok(AttachCommand::Detach) => {
                    let id = next_req_id;
                    let params = SessionsDetachParams {
                        session_id: session_id.to_string(),
                    };
                    let req = Request::new(id, SessionsDetach::NAME, &params)
                        .map_err(|e| format!("encode sessions.detach: {e}"))?;
                    // Best-effort send; we exit even if it fails.
                    let _ = conn.send(&Message::Request(req)).await;
                    return Ok(DriverExit::UserDetach);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Ok(DriverExit::ChannelGone);
                }
            }
        }

        // Wait briefly for either a daemon frame or a new outbound
        // command. We can't await the std mpsc Receiver, so we poll the
        // socket with a short timeout and loop back to drain commands.
        let frame = match tokio::time::timeout(Duration::from_millis(50), conn.recv()).await {
            Ok(Ok(Some(msg))) => msg,
            Ok(Ok(None)) => return Err("daemon closed connection".into()),
            Ok(Err(e)) => return Err(format!("recv: {e}")),
            Err(_) => continue,
        };
        match frame {
            Message::Notification(n) => {
                let Some(params) = n.params else { continue };
                match n.method.as_str() {
                    SessionOutput::NAME => {
                        let payload: SessionOutputParams = match serde_json::from_value(params) {
                            Ok(p) => p,
                            Err(err) => {
                                tracing::warn!(%err, "attach-pump: decode session.output failed");
                                continue;
                            }
                        };
                        if payload.session_id != session_id {
                            continue;
                        }
                        let bytes = match payload.data_bytes() {
                            Ok(b) => b,
                            Err(err) => {
                                tracing::warn!(%err, "attach-pump: base64 decode failed");
                                continue;
                            }
                        };
                        *last_seq = Some(payload.seq);
                        if ev_tx
                            .send(AttachEvent::Bytes {
                                session_id: session_id.to_string(),
                                bytes,
                            })
                            .is_err()
                        {
                            return Ok(DriverExit::ChannelGone);
                        }
                    }
                    SessionGap::NAME => {
                        let payload: SessionGapParams = match serde_json::from_value(params) {
                            Ok(p) => p,
                            Err(err) => {
                                tracing::warn!(%err, "attach-pump: decode session.gap failed");
                                continue;
                            }
                        };
                        if payload.session_id != session_id {
                            continue;
                        }
                        if ev_tx
                            .send(AttachEvent::Gap {
                                session_id: session_id.to_string(),
                                from_seq: payload.from_seq,
                                to_seq: payload.to_seq,
                                dropped_bytes: payload.dropped_bytes,
                            })
                            .is_err()
                        {
                            return Ok(DriverExit::ChannelGone);
                        }
                    }
                    SessionStateNotice::NAME => {
                        let payload: SessionStateParams = match serde_json::from_value(params) {
                            Ok(p) => p,
                            Err(err) => {
                                tracing::warn!(%err, "attach-pump: decode session.state failed");
                                continue;
                            }
                        };
                        if payload.session_id != session_id {
                            continue;
                        }
                        if ev_tx
                            .send(AttachEvent::State {
                                session_id: session_id.to_string(),
                                state: format!("{:?}", payload.state).to_lowercase(),
                                reason: payload.reason,
                            })
                            .is_err()
                        {
                            return Ok(DriverExit::ChannelGone);
                        }
                    }
                    _ => continue,
                }
            }
            Message::Response(_) => {
                // Acks for sessions.attach/write/detach — already handled
                // above by `wait_for_response`, or harmless to drop.
                continue;
            }
            Message::Request(_) => continue,
        }
    }
}

async fn wait_for_response<S>(
    conn: &mut Connection<S>,
    expected: RequestId,
) -> Result<serde_json::Value, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Loop until we see the response matching `expected`. The daemon
    // can interleave notifications with the ack, so we must not assume
    // the very next frame is the response.
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(5), conn.recv()).await {
            Ok(Ok(Some(m))) => m,
            Ok(Ok(None)) => return Err("daemon closed before sessions.attach response".into()),
            Ok(Err(e)) => return Err(format!("recv attach response: {e}")),
            Err(_) => return Err("timed out waiting for sessions.attach response".into()),
        };
        match msg {
            Message::Response(r) if r.id == expected => match r.outcome {
                la_proto::jsonrpc::ResponseOutcome::Result(v) => return Ok(v),
                la_proto::jsonrpc::ResponseOutcome::Error(e) => {
                    return Err(format!("sessions.attach error: {e}"));
                }
            },
            // Drop any unrelated frame that lands before the ack.
            // session.output for our session is still buffered server-
            // side; the catch-up will replay once we set up the loop.
            _ => continue,
        }
    }
}
