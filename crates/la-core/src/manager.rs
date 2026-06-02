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
use crate::worktree::{
    diff::WorktreeLocks, project_slug, DiffEngine, WorktreeManager, WorktreePlan,
};
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
    /// Optional worktree manager. When `Some`, `SessionManager::spawn`
    /// honours [`WorktreeSpawnOptions`] by provisioning a per-session
    /// `git worktree` before forking the PTY child and running the
    /// optional `.lazyagents/hooks/post-create.sh` hook afterwards.
    /// When `None`, every spawn behaves as if `worktree=false` (the
    /// pre-WEK-27 path).
    pub worktree: Option<Arc<WorktreeManager>>,
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
            worktree: None,
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
    /// Worktree path the child was spawned into when `worktree=true`,
    /// echoed so the dispatcher can return it in `SessionsCreateResult`
    /// without a follow-up query. `None` for sessions that share the
    /// project root.
    pub worktree_path: Option<std::path::PathBuf>,
    /// Per-session branch (`la/session-<sid>`), if a worktree was
    /// provisioned. `None` mirrors `worktree_path`.
    pub worktree_branch: Option<String>,
    /// Resolved base branch the worktree was forked from. `None` when
    /// no worktree was provisioned.
    pub base_branch: Option<String>,
    /// Outcome of the optional `.lazyagents/hooks/post-create.sh` hook
    /// — see [`WorktreeManager::run_post_create_hook`]. `None` when no
    /// worktree was provisioned (the hook never runs).
    pub post_create_hook_status: Option<crate::worktree::HookStatus>,
}

/// Knobs passed to [`SessionManager::spawn`] when the caller wants a
/// per-session worktree. `None` ⇒ legacy behaviour (session shares the
/// project root). `Some` requires [`ManagerConfig::worktree`] to also be
/// `Some`; otherwise the manager returns an internal error rather than
/// silently fall back to the no-worktree path.
#[derive(Debug, Clone)]
pub struct WorktreeSpawnOptions {
    /// Project working tree to fork from. Usually the same path the
    /// adapter would use as `cwd` for a no-worktree spawn.
    pub repo_root: std::path::PathBuf,
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
    /// Per-session mutex map for the M2.5 diff review surface (WEK-28).
    /// Stage / unstage / discard / commit take their session's mutex so
    /// concurrent callers don't race on `.git/index.lock`. Reads are
    /// lock-free.
    worktree_locks: WorktreeLocks,
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
                worktree_locks: WorktreeLocks::new(),
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

    /// Look up the `(repo_root, worktree_path, base_branch)` triple for a
    /// session id. `repo_root` is sourced from the project's `root_path`;
    /// `worktree_path` / `base_branch` come from the session row.
    /// Returns `None` when:
    ///
    /// - the session id is unknown,
    /// - the session has no `worktree_path` recorded
    ///   (`worktree: false` at create time, or already cleared by
    ///   archive),
    /// - the owning project can't be looked up (storage error or row
    ///   missing).
    ///
    /// Used by the M2.5 diff RPCs (WEK-28) so the dispatcher can build
    /// a [`DiffEngine`] without threading `Storage` itself.
    pub async fn worktree_for(
        &self,
        id: &SessionId,
    ) -> Option<(std::path::PathBuf, std::path::PathBuf, Option<String>)> {
        let row = self
            .inner
            .storage
            .sessions()
            .get(id.as_str())
            .await
            .ok()??;
        let wt = row.worktree_path?;
        let project = self
            .inner
            .storage
            .projects()
            .get(&row.project_id)
            .await
            .ok()??;
        Some((
            std::path::PathBuf::from(project.root_path),
            std::path::PathBuf::from(wt),
            row.base_branch,
        ))
    }

    /// Build a [`DiffEngine`] bound to a session. Returns `None` under
    /// the same conditions as [`Self::worktree_for`]. The session's
    /// recorded base branch (if any) is threaded into the engine so
    /// `worktree.status` returns real ahead/behind counters instead of
    /// `(0, 0)`.
    pub async fn diff_engine_for(&self, id: &SessionId) -> Option<(DiffEngine, Option<String>)> {
        let (repo_root, worktree_path, base) = self.worktree_for(id).await?;
        Some((
            DiffEngine::new(
                repo_root,
                worktree_path,
                id.clone(),
                self.inner.worktree_locks.clone(),
                base.clone(),
            ),
            base,
        ))
    }

    /// Spawn a new session sharing the project root (no per-session
    /// worktree). Equivalent to
    /// [`spawn_with_options`](Self::spawn_with_options) with `worktree =
    /// None`; kept as a separate entry point because most tests and the
    /// pre-WEK-27 code paths don't care about worktree provisioning.
    pub async fn spawn(
        &self,
        adapter: &dyn AgentAdapter,
        project_id: String,
        request: SpawnRequest,
    ) -> CoreResult<SpawnedSession> {
        self.spawn_with_options(adapter, project_id, request, None)
            .await
    }

    /// Spawn a new session, optionally provisioning a per-session git
    /// worktree.
    ///
    /// Steps (in order, each atomic before the next):
    /// 1. **Worktree** (if `worktree.is_some()`): resolve base branch,
    ///    `git worktree add -b la/session-<sid> <base_sha>`. Failure
    ///    short-circuits before the session row is written, per WEK-8
    ///    brief §2.2 — half-written rows are forbidden.
    /// 2. Adapter builds the [`SpawnSpec`]. `request.cwd` is overridden
    ///    with the worktree path when one was provisioned.
    /// 3. Persist a `starting` row to SQLite (with the worktree fields
    ///    populated if applicable).
    /// 4. Spawn the PTY child and update the row with its pid.
    /// 5. Register the in-memory runtime and start the output pump.
    /// 6. Run the optional `.lazyagents/hooks/post-create.sh` hook (60 s
    ///    budget). Hook failure is advisory and persisted as a separate
    ///    `post_create_hook_status` column — it does NOT mutate the
    ///    session state machine (brief amendment R4).
    /// 7. Publish a `session.state{starting}` event for subscribers.
    pub async fn spawn_with_options(
        &self,
        adapter: &dyn AgentAdapter,
        project_id: String,
        mut request: SpawnRequest,
        worktree: Option<WorktreeSpawnOptions>,
    ) -> CoreResult<SpawnedSession> {
        let backend_id = adapter.descriptor().id.to_string();
        let id = SessionId(la_storage::new_id());

        // ----- Step 1: provision worktree (atomic; nothing else runs
        // until this either succeeds or rolls back) -----
        let worktree_plan = if let Some(opts) = worktree {
            let wt_mgr = self.inner.config.worktree.as_ref().ok_or_else(|| {
                CoreError::Internal(
                    "WorktreeSpawnOptions supplied but ManagerConfig.worktree is None".to_string(),
                )
            })?;
            let base = wt_mgr.resolve_base_branch(&opts.repo_root).await?;
            let slug = project_slug(&opts.repo_root);
            let plan = wt_mgr.create(&opts.repo_root, &slug, &id.0, base).await?;
            // Adapter spawns into the worktree, not the project root.
            request.cwd = plan.path.clone();
            Some((wt_mgr.clone(), opts.repo_root, plan))
        } else {
            None
        };

        let spec = match adapter.spawn_spec(&request) {
            Ok(spec) => spec,
            Err(err) => {
                if let Some((wt_mgr, repo_root, plan)) = &worktree_plan {
                    rollback_worktree(wt_mgr, repo_root, plan).await;
                }
                return Err(err.into());
            }
        };

        let spawn_args = spawn_args_json(&spec);
        let (worktree_path_str, worktree_branch_str, base_branch_str) = worktree_plan
            .as_ref()
            .map(|(_, _, p)| {
                (
                    Some(p.path.to_string_lossy().into_owned()),
                    Some(p.branch.clone()),
                    Some(p.base_branch.clone()),
                )
            })
            .unwrap_or((None, None, None));

        if let Err(err) = self
            .inner
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
                worktree_path: worktree_path_str.clone(),
                worktree_branch: worktree_branch_str.clone(),
                base_branch: base_branch_str.clone(),
                spawn_args,
                origin: "user".to_string(),
                post_create_hook_status: None,
                external_path: None,
            })
            .await
        {
            if let Some((wt_mgr, repo_root, plan)) = &worktree_plan {
                rollback_worktree(wt_mgr, repo_root, plan).await;
            }
            return Err(err.into());
        }

        let pty_size = pty_size_from_spec(&spec, self.inner.config.initial_pty);
        let cmd = command_from_spec(&spec);

        let child = match la_pty::spawn(cmd, pty_size) {
            Ok(child) => child,
            Err(err) => {
                // PTY spawn failed after the row is persisted: leave a
                // dead `starting` row for the orphan reaper, but roll
                // back the worktree so the disk doesn't leak. The
                // sessions row stays so the user can see what failed.
                if let Some((wt_mgr, repo_root, plan)) = &worktree_plan {
                    rollback_worktree(wt_mgr, repo_root, plan).await;
                    let _ = self
                        .inner
                        .storage
                        .sessions()
                        .clear_worktree(id.as_str(), false)
                        .await;
                }
                return Err(err.into());
            }
        };
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

        // Run the post-create hook after the PTY child is alive and the
        // session is in the registry, so the user can interact with the
        // adapter even while their `pnpm install` / `direnv allow` runs.
        // Hook failure is advisory (brief amendment R4): we never roll
        // back the worktree, never tear down the spawn, never mutate
        // `SessionStatus`. The outcome is persisted as a separate
        // column for the TUI to render as a badge.
        let post_create_hook_status =
            if let Some((wt_mgr, repo_root, plan)) = worktree_plan.as_ref() {
                let status = wt_mgr
                    .run_post_create_hook(repo_root, plan, &backend_id, id.as_str())
                    .await;
                if let Err(err) = self
                    .inner
                    .storage
                    .sessions()
                    .set_post_create_hook_status(id.as_str(), status.as_str())
                    .await
                {
                    tracing::warn!(
                        session = %id.0,
                        %err,
                        "post_create_hook_status persist failed"
                    );
                }
                Some(status)
            } else {
                None
            };

        Ok(SpawnedSession {
            id,
            backend: backend_id,
            project_id,
            initial_state: SessionState::Starting,
            pid,
            worktree_path: worktree_plan.as_ref().map(|(_, _, p)| p.path.clone()),
            worktree_branch: worktree_plan.as_ref().map(|(_, _, p)| p.branch.clone()),
            base_branch: worktree_plan
                .as_ref()
                .map(|(_, _, p)| p.base_branch.clone()),
            post_create_hook_status,
        })
    }

    /// Attach a subscriber to a session's output hub.
    ///
    /// `since_seq` is the resume cursor and matches the hub-level
    /// subscription semantics: `None` ⇒ start fresh / live-only, no
    /// catch-up replay; `Some(prev_seq)` ⇒ replay ring chunks whose
    /// `seq > prev_seq`, then continue live. A first-time attacher that
    /// wants whatever is still in the ring can pass `Some(0)` explicitly.
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
    ///
    /// WEK-27: if the session owned a worktree, this also tears the
    /// directory down. The branch is preserved when it carries commits
    /// the user might want to recover (`KeepBranchIfDirty`); otherwise
    /// it's deleted. `worktree_path` is cleared from the row so the
    /// next `sessions.list` doesn't promise something that no longer
    /// exists on disk.
    pub async fn archive(&self, id: &SessionId) -> CoreResult<()> {
        if self.inner.registry.lock().await.contains_key(id) {
            return Err(CoreError::SessionBusy);
        }
        // Snapshot the row BEFORE archive so we can drive worktree
        // cleanup with the original three columns. Doing it after the
        // archive would race against the read, and the row mutation is
        // small enough that the double-fetch isn't worth optimising.
        let row = self.inner.storage.sessions().get(id.as_str()).await?;
        let archived = self.inner.storage.sessions().archive(id.as_str()).await?;
        if !archived {
            return Err(CoreError::SessionNotFound(id.0.clone()));
        }
        if let (Some(wt_mgr), Some(row)) = (self.inner.config.worktree.as_ref(), row.as_ref()) {
            if let (Some(wt_path), Some(branch), Some(base_branch)) = (
                row.worktree_path.as_deref(),
                row.worktree_branch.as_deref(),
                row.base_branch.as_deref(),
            ) {
                let repo_root = self
                    .inner
                    .storage
                    .projects()
                    .get(&row.project_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|p| std::path::PathBuf::from(p.root_path));
                if let Some(repo_root) = repo_root {
                    let handle =
                        WorktreeManager::handle_from_row(repo_root, wt_path, branch, base_branch);
                    match wt_mgr
                        .cleanup(&handle, crate::worktree::CleanupMode::KeepBranchIfDirty)
                        .await
                    {
                        Ok(branch_preserved) => {
                            // Clear the path only after the worktree
                            // is actually gone. WEK-8 §2.4 row 2: when
                            // KeepBranchIfDirty kept the branch (the
                            // agent committed something), the row's
                            // `worktree_branch` column must survive so
                            // the TUI can later offer `git checkout`.
                            // `branch_preserved` is the bit cleanup
                            // already computed — no need to re-derive
                            // it here. On failure we keep the triple so
                            // a future sweep can retry.
                            let _ = self
                                .inner
                                .storage
                                .sessions()
                                .clear_worktree(id.as_str(), branch_preserved)
                                .await;
                        }
                        Err(err) => {
                            tracing::warn!(
                                session = %id.0,
                                %err,
                                "worktree cleanup on archive failed; \
                                 keeping worktree triple on row so a \
                                 future sweep can retry"
                            );
                        }
                    }
                }
            }
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

/// Best-effort worktree rollback used by every error site in
/// [`SessionManager::spawn_with_options`]. Errors are logged but never
/// surfaced — the caller is already returning a typed `CoreError` and
/// the worktree's only debt to the filesystem is the directory + branch
/// pair, both of which `WorktreeManager::cleanup(_, Force)` handles
/// idempotently.
async fn rollback_worktree(
    wt_mgr: &WorktreeManager,
    repo_root: &std::path::Path,
    plan: &WorktreePlan,
) {
    let handle = crate::worktree::WorktreeManager::handle_from_row(
        repo_root.to_path_buf(),
        &plan.path.to_string_lossy(),
        &plan.branch,
        &plan.base_branch,
    );
    if let Err(err) = wt_mgr
        .cleanup(&handle, crate::worktree::CleanupMode::Force)
        .await
    {
        tracing::warn!(
            wt = %plan.path.display(),
            branch = %plan.branch,
            %err,
            "worktree rollback failed (best-effort)"
        );
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
