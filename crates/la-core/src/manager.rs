//! Session manager: ties `la-pty` + `la-adapter` + `la-storage` + the
//! `OutputHub` from `la-ipc` into the daemon-side façade.
//!
//! Responsibilities (from architecture §1.2 + the WEK-18 acceptance list):
//!
//! - `spawn` a new session: ask the adapter for a [`SpawnSpec`], hand it to
//!   `la-pty`, register the resulting child in our registry, persist the
//!   storage row, and drive the per-session output pump.
//! - `attach` / `detach`: hand out [`la_ipc::Subscription`] handles backed
//!   by the per-session hub. Multi-attach is free; the **single-writer**
//!   invariant is enforced here, not in the hub.
//! - `write` / `signal` / `resize`: forward to the live PTY handles after
//!   policy checks.
//! - `archive` / `delete`: lifecycle gates (refuse `delete` on a running
//!   session per §3).
//! - Orphan reaper on startup: any storage row in `starting` / `running`
//!   whose pid is gone gets marked `exited{unknown}`, matching §6.3 "daemon
//!   启动时扫描 sessions".
//!
//! The manager is the **only** writer of `SessionStateChange` to the
//! [`EventBus`] — the per-session output pump publishes state transitions
//! through it so a TUI showing the sessions list updates without needing
//! its own observers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use la_adapter::{AgentAdapter, SpawnRequest, SpawnSpec, StdinMode};
use la_ipc::{HubConfig, OutputHub, SubId, Subscription};
use la_proto::methods::{SessionSignal, SessionState};
use la_proto::notifications::SessionStateParams;
use la_pty::{CommandBuilder, PtyError, PtySize, Signal as PtySignal};
use la_storage::{ChunkKind, NewSession, Storage};
use tokio::sync::{Mutex, RwLock};

use crate::error::CoreResult;
use crate::event_bus::{BusEvent, EventBus};
use crate::session::{SessionId, SessionRuntime, SessionStateChange, SignalFn};
use crate::{CoreError, DEFAULT_RUNNING_PROMOTE, DEFAULT_WAITING_IDLE};

/// Tunables that callers (mostly tests) want to override.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Per-session output-hub config (ring buffer size, sub queue cap,
    /// park duration). Defaults match architecture §6.2.
    pub hub: HubConfig,
    /// Capacity of the global event bus broadcast channel.
    pub bus_capacity: usize,
    /// PTY output silence threshold after which `Running` is downgraded
    /// to `Waiting` (architecture §3 lifecycle).
    pub waiting_idle: Duration,
    /// Idle threshold for the `Starting` → `Running` self-promote when
    /// the backend opens with no output (interactive prompt only).
    pub running_promote: Duration,
    /// Initial PTY size handed to every spawned child unless the adapter
    /// overrides it.
    pub initial_pty: PtySize,
    /// Persist every PTY chunk to the storage `session_chunks` table.
    /// Off by default in tests because it forces a SQLite round-trip per
    /// chunk; on in production so the transcript survives daemon restart.
    pub persist_chunks: bool,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            hub: HubConfig::default(),
            bus_capacity: crate::event_bus::DEFAULT_BUS_CAPACITY,
            waiting_idle: DEFAULT_WAITING_IDLE,
            running_promote: DEFAULT_RUNNING_PROMOTE,
            initial_pty: PtySize::default(),
            persist_chunks: true,
        }
    }
}

/// Public handle returned by [`SessionManager::spawn`].
///
/// The TUI / RPC dispatcher only ever needs the id (everything else flows
/// through `attach`/`write`/`signal`), so we don't expose the runtime
/// internals here.
#[derive(Debug, Clone)]
pub struct SpawnedSession {
    pub id: SessionId,
    pub backend: String,
    pub project_id: String,
    pub initial_state: SessionState,
    pub pid: Option<u32>,
}

/// The composed daemon façade. Cheap to clone — internally an `Arc`.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    storage: Storage,
    bus: EventBus,
    config: ManagerConfig,
    /// Live sessions keyed by id. Wrapped in `Mutex` for end-to-end
    /// exclusivity on multi-step mutations (spawn = insert + start pump);
    /// per-session read-only access borrows briefly inside the lock and
    /// hands out cloneable handles, so contention stays low.
    registry: Mutex<HashMap<SessionId, Arc<RwLock<SessionRuntime>>>>,
}

impl SessionManager {
    /// Build a manager around a ready [`Storage`] (already migrated).
    /// Does NOT perform the orphan-reap scan — call
    /// [`reap_orphans`](Self::reap_orphans) explicitly so tests can choose
    /// their own startup ordering.
    pub fn new(storage: Storage, config: ManagerConfig) -> Self {
        let bus = EventBus::with_capacity(config.bus_capacity);
        Self {
            inner: Arc::new(Inner {
                storage,
                bus,
                config,
                registry: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Clone of the global event bus. The IPC layer subscribes per
    /// connection and serializes events the client asked for.
    pub fn bus(&self) -> EventBus {
        self.inner.bus.clone()
    }

    /// Borrow the underlying storage (mostly for the dispatcher's
    /// `sessions.list` / project queries — the manager doesn't wrap
    /// those because they're pure read paths).
    pub fn storage(&self) -> &Storage {
        &self.inner.storage
    }

    /// Snapshot of currently-tracked session ids. Order is unspecified.
    pub async fn active_ids(&self) -> Vec<SessionId> {
        self.inner.registry.lock().await.keys().cloned().collect()
    }

    /// Number of in-memory sessions; surfaced on `daemon.health`.
    pub async fn active_count(&self) -> usize {
        self.inner.registry.lock().await.len()
    }

    /// Spawn a new session.
    ///
    /// Steps (in order, each idempotent if it fails before the next):
    /// 1. Adapter builds the [`SpawnSpec`].
    /// 2. We persist a `starting` row to SQLite so a daemon crash mid-
    ///    spawn leaves a recoverable trace for the orphan reaper.
    /// 3. We spawn the PTY child and immediately update the storage row
    ///    with its pid.
    /// 4. We register the in-memory runtime and start the output pump.
    /// 5. We publish a `session.state{starting}` event for any subscriber.
    pub async fn spawn(
        &self,
        adapter: &dyn AgentAdapter,
        project_id: String,
        request: SpawnRequest,
    ) -> CoreResult<SpawnedSession> {
        let backend_id = adapter.descriptor().id.to_string();
        let spec = adapter.spawn_spec(&request)?;

        let id = SessionId(la_storage::new_id());
        // Snapshot the spec into the storage row so the import / orphan
        // path can reconstruct enough context to display.
        let spawn_args = spawn_args_json(&spec);

        self.inner
            .storage
            .sessions()
            .create(NewSession {
                id: id.0.clone(),
                project_id: project_id.clone(),
                backend_id: backend_id.clone(),
                external_id: None,
                title: None,
                state: state_str(SessionState::Starting).to_string(),
                pid: None,
                worktree_path: None,
                worktree_branch: None,
                base_branch: None,
                spawn_args,
                origin: "user".to_string(),
            })
            .await?;

        let pty_size = pty_size_from_spec(&spec, self.inner.config.initial_pty);
        let cmd = command_from_spec(&spec);

        let child = la_pty::spawn(cmd, pty_size)?;
        let parts = child.into_parts();
        let pid = parts.pid;

        if let Some(pid) = pid {
            // Best-effort: pid will surface on `sessions.list` even before
            // the session goes Running.
            if let Err(err) = self
                .inner
                .storage
                .sessions()
                .update_pid(&id.0, Some(pid as i64))
                .await
            {
                tracing::warn!(session = %id.0, %err, "pid persist failed");
            }
        }

        let hub = OutputHub::with_config(id.0.clone(), self.inner.config.hub);
        let pid_for_signal = pid;
        let signaller: SignalFn = Box::new(move |sig: PtySignal| match pid_for_signal {
            Some(p) => la_pty::send_signal(p, sig),
            None => Err(PtyError::NoPid),
        });

        let runtime = SessionRuntime {
            hub: hub.clone(),
            writer: parts.writer,
            signaller,
            state: SessionState::Starting,
            exit_code: None,
            writer_holder: None,
            last_output_at: None,
        };
        let runtime = Arc::new(RwLock::new(runtime));

        {
            let mut reg = self.inner.registry.lock().await;
            reg.insert(id.clone(), runtime.clone());
        }

        // Publish Starting up front so a client that attaches immediately
        // sees a `state` event without waiting for the first PTY byte.
        self.publish_state(SessionStateChange {
            id: id.clone(),
            state: SessionState::Starting,
            exit_code: None,
            reason: None,
        });

        // Spawn the per-session output pump. It owns the PTY reader and
        // the `ChildWaiter`; the manager keeps the `OutputHub` and
        // `PtyWriter` so RPC calls can keep reading / writing.
        let pump = OutputPump {
            id: id.clone(),
            manager: self.clone(),
            runtime: runtime.clone(),
            hub: hub.clone(),
            reader: parts.reader,
            waiter: parts.waiter,
            stdin_mode: spec.stdin_mode,
        };
        tokio::spawn(pump.run());

        Ok(SpawnedSession {
            id,
            backend: backend_id,
            project_id,
            initial_state: SessionState::Starting,
            pid,
        })
    }

    /// Attach a subscriber to a session's output hub.
    ///
    /// `since_seq` is the inclusive lower bound for catch-up replay: pass
    /// `None` on first attach to replay everything still in the ring,
    /// pass `Some(prev_seq)` after a reconnect to replay only the gap.
    /// `acquire_input = true` requests writer ownership; if another
    /// client holds it, attach still succeeds and returns
    /// `input_acquired = false`.
    pub async fn attach(
        &self,
        id: &SessionId,
        since_seq: Option<u64>,
        acquire_input: bool,
    ) -> CoreResult<AttachOutcome> {
        let runtime = self.runtime(id).await?;
        let sub = {
            let entry = runtime.read().await;
            entry.hub.subscribe(since_seq).await
        };
        let sub_id = sub.id();

        let mut input_acquired = false;
        if acquire_input {
            let mut entry = runtime.write().await;
            if entry.writer_holder.is_none() {
                entry.writer_holder = Some(sub_id);
                input_acquired = true;
            }
        }

        let snapshot_seq = {
            let entry = runtime.read().await;
            entry.hub.next_seq().await.saturating_sub(1)
        };

        Ok(AttachOutcome {
            subscription: sub,
            snapshot_seq,
            input_acquired,
        })
    }

    /// Drop a subscription (and release writer ownership if held).
    /// Idempotent — calling twice with the same id is harmless.
    pub async fn detach(&self, id: &SessionId, sub: SubId) -> CoreResult<()> {
        let runtime = self.runtime(id).await?;
        {
            let mut entry = runtime.write().await;
            if entry.writer_holder == Some(sub) {
                entry.writer_holder = None;
            }
        }
        let hub = {
            let entry = runtime.read().await;
            entry.hub.clone()
        };
        // Park keeps the queue alive for the configured grace period;
        // `evict_if_still_parked` will reap if the client never returns.
        hub.park(sub).await;
        let evict_hub = hub.clone();
        let park_duration = self.inner.config.hub.park_duration;
        tokio::spawn(async move {
            tokio::time::sleep(park_duration).await;
            let _ = evict_hub.evict_if_still_parked(sub).await;
        });
        Ok(())
    }

    /// Resume a previously-parked subscription. Returns `Ok(None)` if the
    /// park window has passed and the caller must do a fresh `attach`.
    pub async fn resume(
        &self,
        id: &SessionId,
        sub: SubId,
        since_seq: Option<u64>,
    ) -> CoreResult<Option<Subscription>> {
        let runtime = self.runtime(id).await?;
        let hub = {
            let entry = runtime.read().await;
            entry.hub.clone()
        };
        Ok(hub.resume(sub, since_seq).await)
    }

    /// Write bytes to the PTY master. Enforces the single-writer invariant.
    pub async fn write(&self, id: &SessionId, sub: SubId, data: &[u8]) -> CoreResult<()> {
        let runtime = self.runtime(id).await?;
        let writer = {
            let entry = runtime.read().await;
            match entry.writer_holder {
                None => return Err(CoreError::NotAttached),
                Some(holder) if holder != sub => {
                    return Err(CoreError::WriterLocked {
                        holder: holder.get(),
                    });
                }
                Some(_) => {}
            }
            entry.writer.clone()
        };
        writer.write(data.to_vec()).await?;

        if self.inner.config.persist_chunks {
            // Best-effort transcript persistence; errors are logged but
            // don't fail the write (the client already got the byte).
            if let Err(err) = self
                .inner
                .storage
                .chunks()
                .append(id.as_str(), ChunkKind::Input, data)
                .await
            {
                tracing::warn!(session = %id.as_str(), %err, "input chunk persist failed");
            }
        }
        Ok(())
    }

    /// Send a cross-platform signal to the session's child. The signal
    /// vocabulary mirrors `sessions.signal` (Int / Term / Kill).
    pub async fn signal(&self, id: &SessionId, sig: SessionSignal) -> CoreResult<()> {
        let runtime = self.runtime(id).await?;
        let pty_sig = match sig {
            SessionSignal::Int => PtySignal::Interrupt,
            SessionSignal::Term => PtySignal::Terminate,
            SessionSignal::Kill => PtySignal::Kill,
        };
        let entry = runtime.read().await;
        (entry.signaller)(pty_sig)?;
        Ok(())
    }

    /// Archive a session row. Refuses if the session is still in the
    /// active registry — the caller must signal-then-wait first.
    pub async fn archive(&self, id: &SessionId) -> CoreResult<()> {
        if self.inner.registry.lock().await.contains_key(id) {
            return Err(CoreError::SessionBusy);
        }
        let archived = self.inner.storage.sessions().archive(id.as_str()).await?;
        if !archived {
            return Err(CoreError::SessionNotFound(id.0.clone()));
        }
        self.publish_state(SessionStateChange {
            id: id.clone(),
            state: SessionState::Archived,
            exit_code: None,
            reason: None,
        });
        Ok(())
    }

    /// Hard-delete a session row (cascades transcript chunks). Refuses if
    /// the session is still active.
    pub async fn delete(&self, id: &SessionId) -> CoreResult<()> {
        if self.inner.registry.lock().await.contains_key(id) {
            return Err(CoreError::SessionBusy);
        }
        let removed = self.inner.storage.sessions().delete(id.as_str()).await?;
        if !removed {
            return Err(CoreError::SessionNotFound(id.0.clone()));
        }
        Ok(())
    }

    /// Mark any storage row in `starting` / `running` whose pid is no
    /// longer alive as `exited` with no exit code. Returns the count of
    /// rows reaped so the daemon can log it.
    ///
    /// Architecture §6.3: "daemon 启动时扫描 sessions 表 state in
    /// (Running, Starting)、pid 在系统中不存在的，标 Exited{unknown}".
    pub async fn reap_orphans(&self) -> CoreResult<usize> {
        let mut reaped = 0usize;
        let rows = self.inner.storage.sessions().list_active().await?;

        for row in rows {
            let alive = match row.pid {
                Some(pid) => pid_alive(pid as u32),
                None => false,
            };
            if alive {
                continue;
            }
            let updated = self
                .inner
                .storage
                .sessions()
                .update_state(&row.id, state_str(SessionState::Exited), None)
                .await?;
            if updated {
                reaped += 1;
                self.publish_state(SessionStateChange {
                    id: SessionId(row.id),
                    state: SessionState::Exited,
                    exit_code: None,
                    reason: Some("orphan-reap".to_string()),
                });
            }
        }
        Ok(reaped)
    }

    async fn runtime(&self, id: &SessionId) -> CoreResult<Arc<RwLock<SessionRuntime>>> {
        let reg = self.inner.registry.lock().await;
        reg.get(id)
            .cloned()
            .ok_or_else(|| CoreError::SessionNotFound(id.0.clone()))
    }

    /// Internal: publish a [`SessionStateChange`] on the bus as the wire
    /// `session.state` payload. Used by both the manager and the pump.
    pub(crate) fn publish_state(&self, change: SessionStateChange) {
        let params = SessionStateParams {
            session_id: change.id.0.clone(),
            state: change.state,
            exit_code: change.exit_code,
            reason: change.reason,
        };
        self.inner.bus.publish(BusEvent::SessionState(params));
    }
}

/// Result of [`SessionManager::attach`].
pub struct AttachOutcome {
    /// The subscription handle the caller drains to deliver
    /// `session.output` / `session.gap` events to the client.
    pub subscription: Subscription,
    /// `seq` of the last chunk currently in the hub at attach time —
    /// every chunk with `seq <= snapshot_seq` is part of the catch-up
    /// replay; later ones are live.
    pub snapshot_seq: u64,
    /// `true` if the caller requested input ownership and got it.
    pub input_acquired: bool,
}

/// The per-session pump that:
///   - drains the PTY reader,
///   - publishes each chunk on the hub,
///   - infers `Starting → Running → Waiting` transitions,
///   - awaits the child exit and publishes the terminal state.
struct OutputPump {
    id: SessionId,
    manager: SessionManager,
    runtime: Arc<RwLock<SessionRuntime>>,
    hub: OutputHub,
    reader: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    waiter: la_pty::ChildWaiter,
    stdin_mode: StdinMode,
}

impl OutputPump {
    async fn run(mut self) {
        let promote_delay = self.manager.inner.config.running_promote;
        let waiting_idle = self.manager.inner.config.waiting_idle;
        let persist = self.manager.inner.config.persist_chunks;

        // Pre-promote `Starting → Running` after `promote_delay` even if
        // the backend opens silently waiting for input.
        let pre_promote = tokio::time::sleep(promote_delay);
        tokio::pin!(pre_promote);
        let mut idle_tick = tokio::time::interval(waiting_idle.max(Duration::from_millis(50)));
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Burn the immediate first tick — `interval` fires once on creation.
        idle_tick.tick().await;

        loop {
            tokio::select! {
                biased;
                chunk = self.reader.recv() => match chunk {
                    Some(bytes) => {
                        // Update last-output timestamp + maybe promote.
                        let mut now_state = None;
                        {
                            let mut entry = self.runtime.write().await;
                            entry.last_output_at = Some(Instant::now());
                            if entry.state == SessionState::Starting
                                || entry.state == SessionState::Waiting
                            {
                                entry.state = SessionState::Running;
                                now_state = Some(SessionState::Running);
                            }
                        }
                        if let Some(new_state) = now_state {
                            self.persist_state(new_state, None).await;
                            self.manager.publish_state(SessionStateChange {
                                id: self.id.clone(),
                                state: new_state,
                                exit_code: None,
                                reason: None,
                            });
                        }
                        // Fan out to subscribers via the per-session hub.
                        let _seq_range = self.hub.publish(&bytes).await;
                        if persist {
                            if let Err(err) = self
                                .manager
                                .inner
                                .storage
                                .chunks()
                                .append(self.id.as_str(), ChunkKind::Stdout, &bytes[..])
                                .await
                            {
                                tracing::warn!(
                                    session = %self.id.as_str(),
                                    %err,
                                    "output chunk persist failed"
                                );
                            }
                        }
                    }
                    None => break,
                },
                _ = &mut pre_promote => {
                    let mut promoted = false;
                    {
                        let mut entry = self.runtime.write().await;
                        if entry.state == SessionState::Starting {
                            entry.state = SessionState::Running;
                            promoted = true;
                        }
                    }
                    if promoted {
                        self.persist_state(SessionState::Running, None).await;
                        self.manager.publish_state(SessionStateChange {
                            id: self.id.clone(),
                            state: SessionState::Running,
                            exit_code: None,
                            reason: None,
                        });
                    }
                    // Park the sleep for the rest of the loop's life.
                    pre_promote
                        .as_mut()
                        .reset(tokio::time::Instant::now() + Duration::from_secs(86_400));
                }
                _ = idle_tick.tick() => {
                    let mut transitioned = None;
                    {
                        let mut entry = self.runtime.write().await;
                        let idle_long_enough = match entry.last_output_at {
                            Some(at) => at.elapsed() >= waiting_idle,
                            None => false,
                        };
                        if entry.state == SessionState::Running
                            && idle_long_enough
                            && matches!(self.stdin_mode, StdinMode::Pty)
                        {
                            entry.state = SessionState::Waiting;
                            transitioned = Some(SessionState::Waiting);
                        }
                    }
                    if let Some(new_state) = transitioned {
                        self.persist_state(new_state, None).await;
                        self.manager.publish_state(SessionStateChange {
                            id: self.id.clone(),
                            state: new_state,
                            exit_code: None,
                            reason: None,
                        });
                    }
                }
            }
        }

        // Reader drained → child likely exited or someone dropped the
        // writer side. Await the actual exit status so we have a real
        // exit code in the state event.
        let OutputPump {
            id,
            manager,
            runtime,
            hub,
            reader: _,
            waiter,
            stdin_mode: _,
        } = self;
        let exit_code = match waiter.wait().await {
            Ok(status) => Some(status.exit_code() as i32),
            Err(err) => {
                tracing::warn!(session = %id.as_str(), %err, "child wait failed");
                None
            }
        };

        {
            let mut entry = runtime.write().await;
            entry.state = SessionState::Exited;
            entry.exit_code = exit_code;
        }
        if let Err(err) = manager
            .inner
            .storage
            .sessions()
            .update_state(
                id.as_str(),
                state_str(SessionState::Exited),
                exit_code.map(|c| c as i64),
            )
            .await
        {
            tracing::warn!(session = %id.as_str(), %err, "state persist failed");
        }
        manager.publish_state(SessionStateChange {
            id: id.clone(),
            state: SessionState::Exited,
            exit_code,
            reason: None,
        });

        // Tear down: drop from registry + close hub so subscribers see
        // `None` on their next `recv` and detach cleanly.
        {
            let mut reg = manager.inner.registry.lock().await;
            reg.remove(&id);
        }
        hub.close().await;
    }

    async fn persist_state(&self, state: SessionState, exit_code: Option<i32>) {
        if let Err(err) = self
            .manager
            .inner
            .storage
            .sessions()
            .update_state(
                self.id.as_str(),
                state_str(state),
                exit_code.map(|c| c as i64),
            )
            .await
        {
            tracing::warn!(session = %self.id.as_str(), %err, "state persist failed");
        }
    }
}

fn state_str(s: SessionState) -> &'static str {
    match s {
        SessionState::Starting => "starting",
        SessionState::Running => "running",
        SessionState::Waiting => "waiting",
        SessionState::Exited => "exited",
        SessionState::Errored => "errored",
        SessionState::Archived => "archived",
    }
}

fn spawn_args_json(spec: &SpawnSpec) -> serde_json::Value {
    serde_json::json!({
        "program": spec.program.to_string_lossy(),
        "args": spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "cwd": spec.cwd.to_string_lossy(),
        "stdin_mode": match spec.stdin_mode {
            StdinMode::Pty => "pty",
            StdinMode::NullSink => "null",
        },
        "pty": { "cols": spec.pty.cols, "rows": spec.pty.rows },
    })
}

fn pty_size_from_spec(spec: &SpawnSpec, default: PtySize) -> PtySize {
    PtySize {
        rows: if spec.pty.rows > 0 {
            spec.pty.rows
        } else {
            default.rows
        },
        cols: if spec.pty.cols > 0 {
            spec.pty.cols
        } else {
            default.cols
        },
        pixel_width: default.pixel_width,
        pixel_height: default.pixel_height,
    }
}

fn command_from_spec(spec: &SpawnSpec) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&spec.program);
    for a in &spec.args {
        cmd.arg(a);
    }
    cmd.cwd(&spec.cwd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // `kill(pid, 0)` returns EPERM if the process exists but we can't
    // signal it, ESRCH if it's gone. EPERM ⇒ alive for our purposes.
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    matches!(
        kill(Pid::from_raw(pid as i32), None),
        Ok(()) | Err(Errno::EPERM)
    )
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code as i32 == STILL_ACTIVE
    }
}
