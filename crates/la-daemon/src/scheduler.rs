//! Daemon scheduler assembly + run executor (WEK-57 / M3.9).
//!
//! [`SchedulerServices`] is the composition object that ties together:
//!
//! - the [`la_scheduler::SchedulerHandle`] (control channel into the
//!   in-memory heap),
//! - a serialized **run executor** that consumes [`FireEvent`]s,
//!   evaluates the admission gate under a single mutex, persists the
//!   `runs` row, and spawns the session, and
//! - lookup helpers used by `crons.* / runs.*` RPC handlers.
//!
//! ## Why a single admission gate
//!
//! The la-scheduler quota module spells out the TOCTOU window for the
//! per-cron + global concurrency rails: two concurrent fires for the same
//! cron can both snapshot `running_for_cron = 0`, both pass the gate, and
//! then both insert a `runs` row that violates `max_concurrent_runs = 1`.
//! The WEK-57 issue body pins the chosen mitigation as "single admission
//! lock or mpsc executor". We use the lock — the executor loop is already
//! single-task so the lock is only one extra `lock().await` per fire, and
//! it serialises the global `running_global` rail at the same time
//! (whereas a per-cron mutex would still let global slip).
//!
//! ## Lifecycle
//!
//! `SchedulerServices::start` spawns three things tied to the same
//! shutdown notifier:
//!
//! 1. `la_scheduler::Scheduler::start` — the heap loop.
//! 2. A loader task that seeds enabled crons from SQLite into the heap.
//! 3. The fire executor task.
//!
//! On graceful shutdown the executor drains all in-flight admission work
//! before exiting (so a fire that just popped from the channel still gets
//! its `runs` row), then the scheduler control channel is closed.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, TimeZone, Utc};
use la_adapter::{SpawnRequest, StdinMode};
use la_core::{BusEvent, SessionId, SessionManager};
use la_proto::notifications::CronFiredParams;
use la_scheduler::{
    apply_catchup, catchup::CatchupMode, clock::system_clock, evaluate_admission, max_runtime,
    quota::backoff::FailureBackoff, AdmissionDecision, CronQuota, CronSpec, FireEvent, GlobalQuota,
    QuotaSnapshot, Scheduler, SchedulerHandle,
};
use la_storage::{Cron, CronUpsert, NewRejectedRun, NewRun, RunFinish, RunRecord, Storage};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::dispatcher::AdapterRegistry;

/// Default daemon-wide concurrency cap on cron runs. Mirrors
/// `GlobalQuota::default().global_max_concurrent_runs`.
pub const DEFAULT_GLOBAL_MAX_CONCURRENT_RUNS: u32 = 8;

/// Catch-up bound for the same-tick replay path used by `coalesce` mode in
/// `process_fire`. Just a soft ceiling to keep an extreme `min_replay_interval`
/// from runaway iteration; the scheduler already enforces
/// [`la_scheduler::MAX_CATCHUP`] inside its own loop.
const REPLAY_INTERVAL_FLOOR: ChronoDuration = ChronoDuration::seconds(1);

/// All the knobs the run executor needs that aren't already on the cron row.
/// Pulled out so test wiring can shrink the global cap to provoke the
/// concurrency rail without touching every cron row.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    pub global: GlobalQuota,
    /// How often the executor falls back to checking the shutdown
    /// notifier even when the fire channel is quiet. 250 ms keeps the
    /// daemon's §6.4 10 s budget intact without burning CPU.
    pub shutdown_poll: StdDuration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            global: GlobalQuota::default(),
            shutdown_poll: StdDuration::from_millis(250),
        }
    }
}

/// Live scheduler stack. Constructed by [`SchedulerServices::start`] and
/// stored on [`crate::Daemon`] so RPC handlers can reach `handle` and the
/// shutdown sequence can drain `executor_loop`.
pub struct SchedulerServices {
    pub handle: SchedulerHandle,
    /// Hard ceiling on global running cron-spawned runs. Read by RPC
    /// handlers that surface the admission decision (e.g. `crons.run_now`).
    pub config: SchedulerConfig,
    /// Live count of in-flight cron-spawned runs. Bumped by the executor
    /// while the admission lock is held; decremented on terminal finish.
    /// The admission gate snapshot reads from this rather than the
    /// `RunsRepo::count_running_global` query so a freshly-admitted-but-not-
    /// yet-spawned fire still counts towards the cap.
    pub running_global: Arc<Mutex<u32>>,
    /// The single mutex the WEK-57 design centres on. Held across the
    /// snapshot → evaluate → insert window so two fires for any cron
    /// cannot both pass the per-cron / global concurrency rails.
    /// Shared between the executor loop and the `crons.run_now` RPC path.
    pub admission_lock: Arc<Mutex<()>>,
    /// JoinHandles for the two background tasks. Wrapped in Mutex so
    /// [`Self::join_loops`] can take ownership without `&mut self` —
    /// useful because the daemon stores this struct behind `Arc`.
    loops: Mutex<Option<(JoinHandle<()>, JoinHandle<()>)>>,
    /// Executor-only shutdown signal. **Distinct from** the daemon-wide
    /// `Notify` the rest of the runtime shares so the executor doesn't
    /// race the heap loop on shutdown.
    ///
    /// The runtime's connection-drain phase fires the shared notify
    /// early (so dispatcher handlers and accept loop wind down); if the
    /// executor listened on the same signal, it would drain its buffer
    /// and exit while the scheduler heap is still alive — any cron that
    /// reaches its `fire_at` during the §6.4 drain window would push a
    /// `FireEvent` into the channel with no reader, and the fire would
    /// silently disappear. This notifier is fired ONLY from inside
    /// [`Self::shutdown`], strictly *after* the heap loop has been told
    /// to stop pushing new fires and its channel has closed.
    executor_shutdown: Arc<Notify>,
}

impl SchedulerServices {
    /// Boot the scheduler heap, load enabled crons from SQLite, and start
    /// the run executor. Returns once everything is alive.
    ///
    /// `daemon_shutdown` is the workspace-wide notifier (also driving
    /// dispatcher / accept-loop wind-down) — used here ONLY to stop the
    /// diagnostics drain task, which is happy to die early because
    /// dropping `SchedulerChannels::diagnostics` simply closes the
    /// channel. The scheduler heap loop and the run executor use a
    /// separate, internally-owned [`Self::executor_shutdown`] notifier
    /// so they can outlive the daemon's connection-drain phase and
    /// guarantee no scheduled fire is dropped between
    /// `daemon_shutdown.notify_waiters()` and
    /// [`Self::shutdown`].
    pub async fn start(
        storage: Storage,
        manager: SessionManager,
        adapters: AdapterRegistry,
        config: SchedulerConfig,
        daemon_shutdown: Arc<Notify>,
    ) -> Result<Self, SchedulerStartError> {
        let (channels, scheduler_loop) = Scheduler::start(system_clock());
        let handle = channels.handle.clone();
        let fires = channels.fires;
        // diagnostics is consumed by a side task so the bounded channel
        // never blocks the scheduler loop. Today the daemon only logs
        // them; a future status-bar surface can replace this with a
        // bus publisher. The diag drain CAN safely listen on the
        // daemon-wide notify because dropping the channel is a clean
        // exit, unlike a fire we'd be ignoring.
        spawn_diag_drain(channels.diagnostics, daemon_shutdown.clone());

        let running_global = Arc::new(Mutex::new(0_u32));
        let admission_lock = Arc::new(Mutex::new(()));
        let executor_shutdown = Arc::new(Notify::new());

        // Initial cron load. We do this before returning so the daemon's
        // health endpoint and the TUI's first `crons.list` already see the
        // seeded set.
        seed_crons_into_scheduler(&storage, &handle).await?;

        let executor_cfg = ExecutorConfig {
            storage: storage.clone(),
            manager,
            adapters,
            handle: handle.clone(),
            admission_lock: admission_lock.clone(),
            global: config.global,
            running_global: running_global.clone(),
            shutdown_poll: config.shutdown_poll,
        };
        let executor_loop = spawn_executor(executor_cfg, fires, executor_shutdown.clone());

        Ok(Self {
            handle,
            config,
            running_global,
            admission_lock,
            loops: Mutex::new(Some((scheduler_loop, executor_loop))),
            executor_shutdown,
        })
    }

    /// Drain the executor + heap, awaiting both. Idempotent; calling twice
    /// is a no-op because the heap loop has already shut down.
    ///
    /// Ordering matters and is the whole point of the
    /// [`Self::executor_shutdown`] split — see field docs. Concretely:
    ///
    /// 1. Send `Command::Shutdown` to the scheduler heap loop. The
    ///    loop returns immediately, dropping its `FireEvent` sender;
    ///    no further scheduled fires can be produced.
    /// 2. Wait for the heap loop's `JoinHandle` to resolve. Now the
    ///    `mpsc::Receiver<FireEvent>` inside the executor is guaranteed
    ///    to observe `None` once it has drained the buffer.
    /// 3. Notify the executor's private shutdown signal. The executor
    ///    sees this only AFTER it has either drained the channel to
    ///    `None` or popped every remaining buffered fire; either way no
    ///    in-flight admission write is dropped.
    /// 4. Wait for the executor to join.
    ///
    /// The previous design fired a single daemon-wide notify in step 1
    /// and joined in step 4. That left a window — between "accept loop
    /// fires `ctx.shutdown.notify_waiters()`" and "runtime calls
    /// `s.shutdown()`" — during which the heap loop was still alive but
    /// the executor had already exited, silently dropping any fire that
    /// landed in that window.
    pub async fn shutdown(&self) {
        if let Err(err) = self.handle.shutdown().await {
            tracing::debug!(%err, "scheduler control channel shutdown failed");
        }
        let pair = self.loops.lock().await.take();
        if let Some((scheduler_loop, executor_loop)) = pair {
            // First wait for the heap loop to finish; only then can we
            // be sure no more fires will be produced. The executor is
            // still running and will drain whatever the heap pushed
            // before it exited.
            if let Err(err) = scheduler_loop.await {
                tracing::debug!(%err, "scheduler loop join failed");
            }
            // Heap is now gone; tell the executor it can stop after
            // draining any remaining buffered fires.
            self.executor_shutdown.notify_waiters();
            if let Err(err) = executor_loop.await {
                tracing::debug!(%err, "scheduler executor join failed");
            }
        }
    }

    /// Fire the executor shutdown notifier without waiting for the loops
    /// to join. Used in fallback cleanup paths where an outstanding RPC
    /// handler is still holding an `Arc<SchedulerServices>` and we cannot
    /// take ownership for the awaited [`Self::shutdown`] path. The heap
    /// loop is left alone because we don't hold the channel sender; the
    /// owning [`Self::shutdown`] will get to it.
    pub fn request_stop(&self) {
        self.executor_shutdown.notify_waiters();
    }
}

/// Errors surfaced while booting [`SchedulerServices`].
#[derive(Debug, thiserror::Error)]
pub enum SchedulerStartError {
    #[error("storage: {0}")]
    Storage(#[from] la_storage::StorageError),
    #[error("cron seed: {0}")]
    CronSeed(String),
}

/// Map a stored cron row + its previous failure timestamp into the
/// `CronSpec` / `CatchupMode` / mirror state the scheduler heap needs.
fn parse_cron_spec(cron: &Cron) -> Result<(CronSpec, CatchupMode, ChronoDuration), String> {
    let spec = CronSpec::parse(&cron.cron_expr, &cron.tz).map_err(|e| e.to_string())?;
    let mode = match cron.catchup_mode.as_str() {
        "skip" => CatchupMode::Skip,
        "replay" => CatchupMode::Replay,
        _ => CatchupMode::Coalesce,
    };
    // `min_replay_interval` is not stored per-cron yet (architecture §5.3
    // leaves it as a daemon-wide tunable). Default to 1s so a `replay`
    // user that catches up after a long suspend doesn't crash the loop
    // with zero-spaced fires.
    Ok((spec, mode, REPLAY_INTERVAL_FLOOR))
}

async fn seed_crons_into_scheduler(
    storage: &Storage,
    handle: &SchedulerHandle,
) -> Result<(), SchedulerStartError> {
    let all = storage.crons().list().await?;
    let mut seeded = 0_usize;
    let mut skipped = 0_usize;
    for cron in all {
        if cron.enabled == 0 {
            skipped += 1;
            continue;
        }
        let (spec, mode, throttle) = match parse_cron_spec(&cron) {
            Ok(v) => v,
            Err(reason) => {
                tracing::warn!(cron_id = %cron.id, %reason, "skipping enabled cron with bad expr/tz on seed");
                skipped += 1;
                continue;
            }
        };
        let last_fired = cron
            .last_fired_at
            .as_deref()
            .and_then(parse_sqlite_lexical_utc);
        let install_res = handle
            .upsert(cron.id.clone(), spec, mode, throttle, last_fired)
            .await;
        if let Err(err) = install_res {
            return Err(SchedulerStartError::CronSeed(format!(
                "upsert {}: {err}",
                cron.id
            )));
        }
        // Seed the backoff mirror so a daemon restart picks up the floor.
        // Required by the §5.4 contract (la-scheduler module doc).
        if cron.consecutive_failures > 0 {
            let parsed = parse_failure_backoff_or_default(&cron.failure_backoff);
            let last_failure_str = storage
                .runs()
                .last_terminal_failure_at_for_cron(&cron.id)
                .await?;
            let last_failure = last_failure_str
                .as_deref()
                .and_then(parse_sqlite_lexical_utc);
            let _ = handle
                .update_backoff_state(
                    cron.id.clone(),
                    Some(parsed),
                    last_failure,
                    cron.consecutive_failures as u32,
                )
                .await;
        }
        seeded += 1;
    }
    tracing::info!(seeded, skipped, "cron seed complete");
    Ok(())
}

fn parse_failure_backoff_or_default(s: &str) -> FailureBackoff {
    la_scheduler::quota::backoff::parse(s).unwrap_or_default()
}

/// Convert SQLite lexical `YYYY-MM-DD HH:MM:SS` (UTC) to a `DateTime<Utc>`.
fn parse_sqlite_lexical_utc(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

fn format_sqlite_lexical(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

struct ExecutorConfig {
    storage: Storage,
    manager: SessionManager,
    adapters: AdapterRegistry,
    handle: SchedulerHandle,
    admission_lock: Arc<Mutex<()>>,
    global: GlobalQuota,
    running_global: Arc<Mutex<u32>>,
    shutdown_poll: StdDuration,
}

fn spawn_executor(
    cfg: ExecutorConfig,
    mut fires: mpsc::Receiver<FireEvent>,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    // Drain remaining buffered fires so admission writes complete.
                    drain_remaining_fires(&cfg, &mut fires).await;
                    break;
                }
                maybe_fire = fires.recv() => {
                    match maybe_fire {
                        Some(fire) => process_fire(&cfg, fire).await,
                        None => {
                            tracing::info!("fire channel closed; scheduler executor exiting");
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(cfg.shutdown_poll) => {
                    // periodic re-check of shutdown
                }
            }
        }
    })
}

async fn drain_remaining_fires(cfg: &ExecutorConfig, fires: &mut mpsc::Receiver<FireEvent>) {
    while let Ok(fire) = fires.try_recv() {
        process_fire(cfg, fire).await;
    }
}

async fn process_fire(cfg: &ExecutorConfig, fire: FireEvent) {
    // Single-line admission gate: the whole snapshot → evaluate → insert
    // sequence runs under one mutex so two fires for any cron cannot
    // each see `running_for_cron = 0` and both pass.
    let _gate = cfg.admission_lock.lock().await;

    let cron = match cfg.storage.crons().get(&fire.cron_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::warn!(cron_id = %fire.cron_id, "fire for unknown cron; ignoring");
            return;
        }
        Err(err) => {
            tracing::warn!(cron_id = %fire.cron_id, %err, "cron lookup failed; ignoring fire");
            return;
        }
    };

    if cron.enabled == 0 {
        tracing::debug!(cron_id = %cron.id, "fire while disabled; admission will refuse");
    }

    // Build the snapshot from authoritative sources. `running_global`
    // comes from the in-memory counter so a spawning-but-not-yet-DB-
    // visible run is still counted; everything else comes from SQLite.
    let snapshot = match build_snapshot(cfg, &cron, fire.fired_at).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(cron_id = %cron.id, %err, "snapshot build failed; ignoring fire");
            return;
        }
    };
    let quota = cron_to_quota(&cron);

    let decision = evaluate_admission(&quota, &cfg.global, &snapshot);

    if let AdmissionDecision::Admit = decision {
        if let Err(err) = admit_and_spawn(cfg, &cron, &fire).await {
            tracing::warn!(cron_id = %cron.id, %err, "admit failed");
        }
    } else if decision.is_deferral() {
        // Deferrals (loadavg, backoff) skip the audit row but we still
        // bump `last_fired_at` so we don't replay-fire next tick. We do
        // NOT touch the row, only log.
        tracing::info!(
            cron_id = %cron.id,
            reason = ?decision.error_kind(),
            detail = %decision.error_detail(),
            "fire deferred"
        );
    } else {
        write_rejected_audit(cfg, &cron, &fire, decision).await;
    }

    // Advance the cron watermark for every *scheduled* outcome — admit,
    // refuse, defer alike — so a daemon restart resumes from the most
    // recent tick rather than catching up past it. Without this, a fire
    // that wrote an audit row (refuse) or only logged (defer) would be
    // re-processed after restart and could spawn a real run once the
    // quota is back under the cap. `run_now` calls go through
    // `admit_and_spawn_with_id` directly and intentionally skip this so
    // a manual trigger never consumes a scheduled tick.
    persist_fire_watermark(cfg, &cron, &fire).await;
}

async fn build_snapshot(
    cfg: &ExecutorConfig,
    cron: &Cron,
    now: DateTime<Utc>,
) -> Result<QuotaSnapshot, la_storage::StorageError> {
    let since_dt = now - ChronoDuration::hours(24);
    let since_lex = format_sqlite_lexical(since_dt);
    let running_for_cron = cfg.storage.runs().count_running_for_cron(&cron.id).await? as u32;
    let window_runs = cfg
        .storage
        .runs()
        .count_since_for_cron(&cron.id, &since_lex)
        .await? as u32;
    let window_cost = cfg
        .storage
        .runs()
        .sum_cost_since_for_cron(&cron.id, &since_lex)
        .await?;
    let last_failure_str = cfg
        .storage
        .runs()
        .last_terminal_failure_at_for_cron(&cron.id)
        .await?;
    let last_failure = last_failure_str
        .as_deref()
        .and_then(parse_sqlite_lexical_utc);
    let running_global = *cfg.running_global.lock().await;
    Ok(QuotaSnapshot {
        running_for_cron,
        running_global,
        window_runs_today: window_runs,
        window_cost_today: window_cost,
        current_loadavg_1m: la_scheduler::quota::loadavg::sample_loadavg_1m(),
        now,
        last_terminal_failure_at: last_failure,
    })
}

fn cron_to_quota(cron: &Cron) -> CronQuota {
    CronQuota {
        max_concurrent_runs: cron.max_concurrent_runs.max(0) as u32,
        max_runs_per_day: cron.max_runs_per_day.max(0) as u32,
        max_runtime_s: cron.max_runtime_s.max(0) as u32,
        cost_budget_usd_per_day: cron.cost_budget_usd_per_day,
        pause_on_consecutive_failures: cron.pause_on_consecutive_failures.max(0) as u32,
        consecutive_failures: cron.consecutive_failures.max(0) as u32,
        failure_backoff: Some(parse_failure_backoff_or_default(&cron.failure_backoff)),
        enabled: cron.enabled != 0,
    }
}

async fn admit_and_spawn(
    cfg: &ExecutorConfig,
    cron: &Cron,
    fire: &FireEvent,
) -> Result<String, AdmitError> {
    admit_and_spawn_with_id(cfg, cron, fire, None).await
}

async fn admit_and_spawn_with_id(
    cfg: &ExecutorConfig,
    cron: &Cron,
    fire: &FireEvent,
    out_run_id: Option<Arc<Mutex<String>>>,
) -> Result<String, AdmitError> {
    // 1. Reserve the global slot under the admission lock so the next
    //    fire sees the updated count even before the runs row exists.
    {
        let mut count = cfg.running_global.lock().await;
        *count = count.saturating_add(1);
    }

    // 2. Insert the `runs` row (`spawning`). The admission lock is still
    //    held by the outer `process_fire`; this is the canonical write.
    let run_id = la_storage::new_id();
    let new_run = NewRun {
        id: run_id.clone(),
        cron_id: Some(cron.id.clone()),
        session_id: None,
        scheduled_at: format_sqlite_lexical(fire.scheduled_at),
        started_at: Some(format_sqlite_lexical(fire.fired_at)),
        status: "spawning".to_string(),
        coalesced_count: fire.coalesced_count.max(1) as i64,
    };
    if let Err(e) = cfg.storage.runs().create(new_run).await {
        // Roll back the in-memory counter on storage error so a wedged
        // SQLite doesn't permanently inflate the global rail.
        decrement_global(cfg).await;
        return Err(AdmitError::Storage(e));
    }

    // NB: `crons.last_fired_at` / `next_fire_at` are advanced by the
    // *scheduled* caller (`process_fire`), not here. `crons.run_now` also
    // funnels through this helper and must NOT bump the cron watermark —
    // a manual trigger is out-of-band and should not consume a scheduled
    // tick.

    // 3. Look up the adapter; bail with an audit row if it disappeared.
    let adapter = match cfg.adapters.get(&cron.backend_id) {
        Some(a) => a,
        None => {
            finish_run_with_error(
                cfg,
                &run_id,
                "failed",
                "adapter_missing",
                &format!("backend {:?} not registered", cron.backend_id),
            )
            .await;
            decrement_global(cfg).await;
            return Err(AdmitError::AdapterMissing(cron.backend_id.clone()));
        }
    };

    // 4. Resolve the project root for cwd.
    let project_root = match cfg.storage.projects().get(&cron.project_id).await {
        Ok(Some(p)) => p.root_path,
        _ => {
            finish_run_with_error(
                cfg,
                &run_id,
                "failed",
                "project_missing",
                &format!("project {} not found", cron.project_id),
            )
            .await;
            decrement_global(cfg).await;
            return Err(AdmitError::ProjectMissing(cron.project_id.clone()));
        }
    };

    let mut request = SpawnRequest::new(project_root.clone());
    request.prompt = Some(cron.prompt.clone());
    request.stdin_mode = StdinMode::Pty;
    // Forward extra args from spawn_args.args if present.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&cron.spawn_args) {
        if let Some(args) = parsed.get("args").and_then(|v| v.as_array()) {
            for a in args {
                if let Some(s) = a.as_str() {
                    request.extra_args.push(std::ffi::OsString::from(s));
                }
            }
        }
    }

    // 5. Spawn the session.
    let spawned = match cfg
        .manager
        .spawn(&*adapter, cron.project_id.clone(), request)
        .await
    {
        Ok(s) => s,
        Err(err) => {
            let msg = err.to_string();
            finish_run_with_error(cfg, &run_id, "failed", "spawn_failed", &msg).await;
            decrement_global(cfg).await;
            return Err(AdmitError::Spawn(msg));
        }
    };

    // 6. Stamp session_id + flip status to running.
    let _ = cfg
        .storage
        .runs()
        .attach_session(&run_id, spawned.id.as_str())
        .await;
    let _ = cfg.storage.runs().update_status(&run_id, "running").await;

    // 7. Publish a cron.fired pulse so subscribed TUIs (M3.6 status bar)
    //    can render the pulse animation.
    cfg.manager
        .bus()
        .publish(BusEvent::CronFired(CronFiredParams {
            cron_id: cron.id.clone(),
            run_id: run_id.clone(),
            fired_at: fire.fired_at.to_rfc3339(),
            status: "running".to_string(),
        }));

    // 8. Spawn the run-completion watcher.
    spawn_run_watcher(cfg, run_id.clone(), spawned.id, cron.clone());

    if let Some(holder) = out_run_id {
        *holder.lock().await = run_id.clone();
    }
    Ok(run_id)
}

#[derive(Debug, thiserror::Error)]
enum AdmitError {
    #[error("storage: {0}")]
    Storage(#[from] la_storage::StorageError),
    #[error("adapter missing: {0}")]
    AdapterMissing(String),
    #[error("project missing: {0}")]
    ProjectMissing(String),
    #[error("spawn: {0}")]
    Spawn(String),
}

async fn decrement_global(cfg: &ExecutorConfig) {
    let mut count = cfg.running_global.lock().await;
    *count = count.saturating_sub(1);
}

/// Bump `crons.last_fired_at` / `next_fire_at` so daemon restart does not
/// catch-up-replay the same scheduled tick. Called for admit / refuse /
/// defer of any *scheduled* fire; `run_now` fires skip this so an out-of-band
/// manual trigger never advances the cron-watermark.
async fn persist_fire_watermark(cfg: &ExecutorConfig, cron: &Cron, fire: &FireEvent) {
    let next_fire_at = match parse_cron_spec(cron) {
        Ok((spec, _, _)) => spec.next_after(fire.fired_at).map(format_sqlite_lexical),
        Err(_) => None,
    };
    let _ = cfg
        .storage
        .crons()
        .mark_fired(
            &cron.id,
            &format_sqlite_lexical(fire.fired_at),
            next_fire_at.as_deref(),
        )
        .await;
}

async fn finish_run_with_error(
    cfg: &ExecutorConfig,
    run_id: &str,
    status: &str,
    error_kind: &str,
    error_detail: &str,
) {
    let now = format_sqlite_lexical(Utc::now());
    let finish = RunFinish {
        finished_at: now,
        status: status.to_string(),
        exit_code: None,
        cost_usd_est: None,
        error_kind: Some(error_kind.to_string()),
        error_detail: Some(error_detail.to_string()),
        tail_log: None,
    };
    if let Err(err) = cfg.storage.runs().finish(run_id, finish).await {
        tracing::warn!(%run_id, %err, "finish_run_with_error failed");
    }
}

async fn write_rejected_audit(
    cfg: &ExecutorConfig,
    cron: &Cron,
    fire: &FireEvent,
    decision: AdmissionDecision,
) {
    let Some(status) = decision.rejected_status() else {
        return;
    };
    let Some(error_kind) = decision.error_kind() else {
        return;
    };
    let detail = decision.error_detail();
    let rejected = NewRejectedRun {
        id: &la_storage::new_id(),
        cron_id: &cron.id,
        scheduled_at: &format_sqlite_lexical(fire.scheduled_at),
        status,
        coalesced_count: fire.coalesced_count.max(1) as i64,
        error_kind,
        error_detail: &detail,
    };
    if let Err(err) = cfg.storage.runs().create_rejected(rejected).await {
        tracing::warn!(cron_id = %cron.id, %err, "audit-row insert failed");
    } else {
        tracing::debug!(cron_id = %cron.id, ?error_kind, "fire refused; audit row written");
    }
}

fn spawn_run_watcher(cfg: &ExecutorConfig, run_id: String, session_id: SessionId, cron: Cron) {
    let storage = cfg.storage.clone();
    let manager = cfg.manager.clone();
    let handle = cfg.handle.clone();
    let running_global = cfg.running_global.clone();
    let max_rt = max_runtime(&cron_to_quota(&cron));
    tokio::spawn(async move {
        // Poll the session row for terminal state. la-core doesn't expose
        // a per-session exit future yet; the bus delivers SessionState
        // events but a subscribe-per-watcher is heavier than a 1s poll.
        let start = std::time::Instant::now();
        let outcome = loop {
            tokio::time::sleep(StdDuration::from_secs(1)).await;
            let row = match storage.sessions().get(session_id.as_str()).await {
                Ok(Some(r)) => r,
                _ => {
                    // session row vanished — record an error and bail.
                    break TerminalOutcome {
                        status: "failed".to_string(),
                        exit_code: None,
                        error_kind: Some("session_missing".to_string()),
                        error_detail: Some(format!("session {} vanished", session_id.as_str())),
                    };
                }
            };
            match row.state.as_str() {
                "exited" => {
                    let exit_code = row.exit_code;
                    let (status, ek, ed) = match exit_code {
                        Some(0) => ("completed".to_string(), None, None),
                        Some(code) => (
                            "failed".to_string(),
                            Some("exit_code".to_string()),
                            Some(format!("exit code {code}")),
                        ),
                        None => (
                            "failed".to_string(),
                            Some("signaled".to_string()),
                            Some("session exited without a code".to_string()),
                        ),
                    };
                    break TerminalOutcome {
                        status,
                        exit_code,
                        error_kind: ek,
                        error_detail: ed,
                    };
                }
                "errored" => {
                    break TerminalOutcome {
                        status: "failed".to_string(),
                        exit_code: row.exit_code,
                        error_kind: Some("session_errored".to_string()),
                        error_detail: None,
                    };
                }
                _ => {}
            }
            if let Some(rt) = max_rt {
                if start.elapsed() >= rt {
                    let _ = manager
                        .signal(&session_id, la_proto::methods::SessionSignal::Term)
                        .await;
                    break TerminalOutcome {
                        status: "timed_out".to_string(),
                        exit_code: None,
                        error_kind: Some("max_runtime_exceeded".to_string()),
                        error_detail: Some(format!("max_runtime_s={} exceeded", rt.as_secs())),
                    };
                }
            }
        };

        let finish = RunFinish {
            finished_at: format_sqlite_lexical(Utc::now()),
            status: outcome.status.clone(),
            exit_code: outcome.exit_code,
            cost_usd_est: None,
            error_kind: outcome.error_kind.clone(),
            error_detail: outcome.error_detail.clone(),
            tail_log: None,
        };
        if let Err(err) = storage.runs().finish(&run_id, finish).await {
            tracing::warn!(%run_id, %err, "run finish persist failed");
        }

        // Update consecutive_failures + scheduler backoff mirror.
        if matches!(outcome.status.as_str(), "failed" | "timed_out") {
            match storage.crons().bump_consecutive_failures(&cron.id).await {
                Ok(Some(after)) => {
                    let parsed = parse_failure_backoff_or_default(&cron.failure_backoff);
                    let now = Utc::now();
                    let _ = handle
                        .update_backoff_state(
                            cron.id.clone(),
                            Some(parsed),
                            Some(now),
                            after as u32,
                        )
                        .await;
                    if la_scheduler::should_auto_pause(
                        cron.pause_on_consecutive_failures.max(0) as u32,
                        after as u32,
                    ) {
                        match storage.crons().pause_for_failures(&cron.id).await {
                            Ok(true) => {
                                tracing::warn!(cron_id = %cron.id, "cron auto-paused on consecutive failures");
                            }
                            Ok(false) => {}
                            Err(err) => {
                                tracing::warn!(cron_id = %cron.id, %err, "pause_for_failures failed");
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(cron_id = %cron.id, %err, "bump consecutive_failures failed")
                }
            }
        } else if outcome.status == "completed" {
            let _ = storage.crons().reset_consecutive_failures(&cron.id).await;
            let _ = handle.clear_backoff_state(cron.id.clone()).await;
        }

        decrement_global_owned(&running_global).await;
    });
}

async fn decrement_global_owned(counter: &Arc<Mutex<u32>>) {
    let mut count = counter.lock().await;
    *count = count.saturating_sub(1);
}

struct TerminalOutcome {
    status: String,
    exit_code: Option<i64>,
    error_kind: Option<String>,
    error_detail: Option<String>,
}

fn spawn_diag_drain(mut diag: mpsc::Receiver<la_scheduler::SchedulerEvent>, shutdown: Arc<Notify>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.notified() => break,
                ev = diag.recv() => {
                    match ev {
                        Some(la_scheduler::SchedulerEvent::ClockSkewDetected { skew_seconds, recomputed_entries }) => {
                            tracing::warn!(skew_seconds, recomputed_entries, "scheduler clock skew");
                        }
                        None => break,
                    }
                }
            }
        }
    });
}

// ===========================================================================
// Public CRUD helpers used by the IPC dispatcher. Keeping them here means
// every cron mutation goes through one place that ALSO drives the heap.
// ===========================================================================

/// Apply a [`CronUpsert`] to storage AND re-install the entry in the
/// scheduler heap. Failures roll back neither side; the scheduler error is
/// surfaced so the dispatcher can return `CRON_INVALID_EXPR` /
/// `CRON_INVALID_TZ` as appropriate.
pub async fn upsert_cron(
    services: &SchedulerServices,
    storage: &Storage,
    upsert: CronUpsert,
) -> Result<Cron, CronOpError> {
    // Pre-parse so a bad expr/tz never lands a heap-less row.
    let spec = CronSpec::parse(&upsert.cron_expr, &upsert.tz).map_err(|err| match err {
        la_scheduler::Error::InvalidExpr { reason, .. } => CronOpError::InvalidExpr(reason),
        la_scheduler::Error::InvalidTimezone(tz) => CronOpError::InvalidTz(tz),
        other => CronOpError::Other(other.to_string()),
    })?;
    let mode = match upsert.catchup_mode.as_str() {
        "skip" => CatchupMode::Skip,
        "replay" => CatchupMode::Replay,
        _ => CatchupMode::Coalesce,
    };
    let enabled = upsert.enabled;
    let cron = storage.crons().upsert(upsert).await?;
    if enabled {
        services
            .handle
            .upsert(
                cron.id.clone(),
                spec,
                mode,
                REPLAY_INTERVAL_FLOOR,
                cron.last_fired_at
                    .as_deref()
                    .and_then(parse_sqlite_lexical_utc),
            )
            .await
            .map_err(|e| CronOpError::Other(e.to_string()))?;
    } else {
        let _ = services.handle.delete(cron.id.clone()).await;
    }
    Ok(cron)
}

pub async fn delete_cron(
    services: &SchedulerServices,
    storage: &Storage,
    cron_id: &str,
) -> Result<bool, CronOpError> {
    let removed = storage.crons().delete(cron_id).await?;
    let _ = services.handle.delete(cron_id.to_string()).await;
    Ok(removed)
}

pub async fn set_enabled(
    services: &SchedulerServices,
    storage: &Storage,
    cron_id: &str,
    enabled: bool,
) -> Result<Cron, CronOpError> {
    let _ = storage.crons().set_enabled(cron_id, enabled).await?;
    let cron = storage
        .crons()
        .get(cron_id)
        .await?
        .ok_or_else(|| CronOpError::NotFound(cron_id.to_string()))?;
    if enabled {
        let (spec, mode, throttle) = parse_cron_spec(&cron).map_err(CronOpError::Other)?;
        services
            .handle
            .upsert(
                cron.id.clone(),
                spec,
                mode,
                throttle,
                cron.last_fired_at
                    .as_deref()
                    .and_then(parse_sqlite_lexical_utc),
            )
            .await
            .map_err(|e| CronOpError::Other(e.to_string()))?;
    } else {
        let _ = services.handle.delete(cron.id.clone()).await;
    }
    Ok(cron)
}

/// Fire a cron immediately, going through the same admission gate as a
/// scheduled fire. Returns the new run_id when admitted; `None` (with the
/// reason) when refused.
pub async fn run_now(
    services: &SchedulerServices,
    storage: &Storage,
    adapters: &AdapterRegistry,
    manager: &SessionManager,
    cron_id: &str,
) -> Result<RunNowOutcome, CronOpError> {
    let cron = storage
        .crons()
        .get(cron_id)
        .await?
        .ok_or_else(|| CronOpError::NotFound(cron_id.to_string()))?;
    let now = Utc::now();
    let exec_cfg = ExecutorConfig {
        storage: storage.clone(),
        manager: manager.clone(),
        adapters: adapters.clone(),
        handle: services.handle.clone(),
        admission_lock: services.admission_lock.clone(),
        global: services.config.global,
        running_global: services.running_global.clone(),
        shutdown_poll: services.config.shutdown_poll,
    };
    let fire = FireEvent {
        cron_id: cron.id.clone(),
        scheduled_at: now,
        fired_at: now,
        coalesced_count: 1,
        catchup_degraded: false,
    };

    // Share the admission lock with the executor loop so a scheduled fire
    // and a `run_now` cannot both pass `max_concurrent_runs=1`.
    let _gate = services.admission_lock.lock().await;
    let snapshot = build_snapshot(&exec_cfg, &cron, now)
        .await
        .map_err(CronOpError::Storage)?;
    let quota = cron_to_quota(&cron);
    let decision = evaluate_admission(&quota, &services.config.global, &snapshot);
    match decision {
        AdmissionDecision::Admit => {
            let run_id_holder = Arc::new(Mutex::new(String::new()));
            admit_and_spawn_with_id(&exec_cfg, &cron, &fire, Some(run_id_holder.clone()))
                .await
                .map_err(|e| CronOpError::Other(e.to_string()))?;
            let id = run_id_holder.lock().await.clone();
            Ok(RunNowOutcome::Admitted { run_id: id })
        }
        other => {
            // Match the executor's behaviour: persist an audit row for
            // every non-deferral refusal so `runs.list` reflects the
            // attempt and the user sees why the run never spawned.
            if !other.is_deferral() {
                write_rejected_audit(&exec_cfg, &cron, &fire, other).await;
            }
            Ok(RunNowOutcome::Refused {
                reason: other.error_kind().unwrap_or("quota_unknown").to_string(),
                detail: other.error_detail(),
            })
        }
    }
}

pub enum RunNowOutcome {
    Admitted { run_id: String },
    Refused { reason: String, detail: String },
}

#[derive(Debug, thiserror::Error)]
pub enum CronOpError {
    #[error("cron not found: {0}")]
    NotFound(String),
    #[error("invalid cron expression: {0}")]
    InvalidExpr(String),
    #[error("invalid timezone: {0}")]
    InvalidTz(String),
    #[error("storage: {0}")]
    Storage(#[from] la_storage::StorageError),
    #[error("{0}")]
    Other(String),
}

/// Cron → wire `CronEntry` conversion used by `crons.list / get / upsert /
/// set_enabled`.
pub fn cron_to_wire(cron: Cron) -> la_proto::methods::CronEntry {
    let spawn_args: serde_json::Value =
        serde_json::from_str(&cron.spawn_args).unwrap_or(serde_json::json!({}));
    la_proto::methods::CronEntry {
        id: cron.id,
        name: cron.name,
        enabled: cron.enabled != 0,
        project_id: cron.project_id,
        backend: cron.backend_id,
        spawn_args,
        prompt: cron.prompt,
        cron_expr: cron.cron_expr,
        tz: cron.tz,
        catchup_mode: cron.catchup_mode,
        max_concurrent_runs: cron.max_concurrent_runs.max(0) as u32,
        max_runs_per_day: cron.max_runs_per_day.max(0) as u32,
        max_runtime_s: cron.max_runtime_s.max(0) as u32,
        cost_budget_usd_per_day: cron.cost_budget_usd_per_day,
        failure_backoff: cron.failure_backoff,
        pause_on_consecutive_failures: cron.pause_on_consecutive_failures.max(0) as u32,
        consecutive_failures: cron.consecutive_failures.max(0) as u32,
        last_fired_at: cron.last_fired_at.and_then(sqlite_lex_to_rfc3339_opt),
        next_fire_at: cron.next_fire_at.and_then(sqlite_lex_to_rfc3339_opt),
        created_at: sqlite_lex_to_rfc3339_or_pass(cron.created_at),
        updated_at: sqlite_lex_to_rfc3339_or_pass(cron.updated_at),
    }
}

pub fn run_to_wire(run: RunRecord) -> la_proto::methods::RunEntry {
    la_proto::methods::RunEntry {
        id: run.id,
        cron_id: run.cron_id,
        session_id: run.session_id,
        scheduled_at: sqlite_lex_to_rfc3339_or_pass(run.scheduled_at),
        started_at: run.started_at.and_then(sqlite_lex_to_rfc3339_opt),
        finished_at: run.finished_at.and_then(sqlite_lex_to_rfc3339_opt),
        status: run.status,
        exit_code: run.exit_code,
        coalesced_count: run.coalesced_count.max(0) as u32,
        cost_usd_est: run.cost_usd_est,
        error_kind: run.error_kind,
        error_detail: run.error_detail,
    }
}

fn sqlite_lex_to_rfc3339_opt(s: String) -> Option<String> {
    parse_sqlite_lexical_utc(&s).map(|dt| dt.to_rfc3339())
}

fn sqlite_lex_to_rfc3339_or_pass(s: String) -> String {
    sqlite_lex_to_rfc3339_opt(s.clone()).unwrap_or(s)
}

/// Pure preview path for `crons.dry_run`: parse a cron expression + tz and
/// project the next `count` fire times (capped at 20).
pub fn dry_run_fires(expr: &str, tz: &str, count: u32) -> Result<Vec<DateTime<Utc>>, CronOpError> {
    let spec = CronSpec::parse(expr, tz).map_err(|err| match err {
        la_scheduler::Error::InvalidExpr { reason, .. } => CronOpError::InvalidExpr(reason),
        la_scheduler::Error::InvalidTimezone(tz) => CronOpError::InvalidTz(tz),
        other => CronOpError::Other(other.to_string()),
    })?;
    let mut out = Vec::with_capacity(count.min(20) as usize);
    let mut cursor = Utc::now();
    for _ in 0..count.min(20) {
        if let Some(next) = spec.next_after(cursor) {
            out.push(next);
            cursor = next;
        } else {
            break;
        }
    }
    Ok(out)
}

// `apply_catchup` re-export pin to silence the unused import warning under
// the `dead_code` lint when the executor doesn't reach the catch-up emission
// path itself (the heap loop does). Keep until WEK-58 wires the catch-up
// resolver into the run_now / coalesce paths the dispatcher exposes.
#[allow(dead_code)]
fn _force_link_catchup() {
    let _ = apply_catchup;
}

#[cfg(test)]
mod tests {
    //! Unit tests for the daemon scheduler's admission gate invariants
    //! that are hard to exercise through the full daemon IPC harness.
    //!
    //! For end-to-end coverage of `crons.*` / `runs.*` RPC, see
    //! `tests/wek57_scheduler.rs`.

    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use la_adapter::{AdapterDescriptor, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec};
    use la_core::ManagerConfig;
    use la_scheduler::clock::system_clock;
    use la_storage::{BackendUpsert, CronUpsert, NewProject, Storage, StorageConfig};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Adapter that just hands back `/bin/true` so an unintended spawn
    /// completes immediately. Never reached by the leak-on-storage-error
    /// test because the storage failure aborts before the adapter is
    /// invoked, but kept honest in case the assertion ordering changes.
    struct NoopAdapter;

    #[async_trait]
    impl AgentAdapter for NoopAdapter {
        fn descriptor(&self) -> AdapterDescriptor {
            AdapterDescriptor {
                id: "noop",
                display_name: "Noop Adapter",
                default_program: "/bin/true",
                docs_url: "https://example.test/noop",
            }
        }

        async fn probe(&self) -> ProbeResult {
            ProbeResult::Available {
                version: "0".into(),
            }
        }

        fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, la_adapter::AdapterError> {
            Ok(SpawnSpec {
                program: PathBuf::from("/bin/true"),
                args: vec![],
                env: req.env.clone(),
                cwd: req.cwd.clone(),
                pty: req.pty,
                stdin_mode: req.stdin_mode,
            })
        }

        fn encode_user_input(&self, text: &str) -> Bytes {
            Bytes::copy_from_slice(text.as_bytes())
        }
    }

    /// Build a [`Storage`] with one project + one enabled cron so we can
    /// drive the admission gate without booting the whole daemon.
    async fn fixture() -> (
        TempDir,
        Storage,
        AdapterRegistry,
        la_core::SessionManager,
        Cron,
    ) {
        let dir = TempDir::new().expect("tempdir");
        let storage = Storage::open(StorageConfig::for_test(dir.path()))
            .await
            .expect("open storage");
        storage
            .backends()
            .upsert(BackendUpsert {
                id: "noop",
                display_name: "Noop Adapter",
                version: None,
                available: true,
            })
            .await
            .expect("backend");
        let project_id = la_storage::new_id();
        storage
            .projects()
            .create(NewProject {
                id: project_id.clone(),
                root_path: dir.path().display().to_string(),
                display_name: "leak-test".into(),
                vcs: None,
            })
            .await
            .expect("project");
        let cron_id = la_storage::new_id();
        let cron = storage
            .crons()
            .upsert(CronUpsert {
                id: cron_id.clone(),
                name: "leak".into(),
                enabled: true,
                project_id,
                backend_id: "noop".into(),
                spawn_args: serde_json::json!({}),
                prompt: "noop".into(),
                cron_expr: "0 0 1 1 *".into(),
                tz: "UTC".into(),
                catchup_mode: "coalesce".into(),
                max_concurrent_runs: 1,
                max_runs_per_day: 24,
                max_runtime_s: 60,
                cost_budget_usd_per_day: None,
                failure_backoff: "expo(1m,2,1h)".into(),
                pause_on_consecutive_failures: 5,
                consecutive_failures: 0,
                last_fired_at: None,
                next_fire_at: None,
            })
            .await
            .expect("cron");
        let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
        adapters.insert("noop".into(), Arc::new(NoopAdapter));
        let registry = AdapterRegistry::from_map(adapters);
        let manager = la_core::SessionManager::new(storage.clone(), ManagerConfig::default());
        (dir, storage, registry, manager, cron)
    }

    /// Build the executor config that `process_fire` / `admit_and_spawn`
    /// consume.
    fn exec_cfg(
        storage: &Storage,
        adapters: &AdapterRegistry,
        manager: &la_core::SessionManager,
        running_global: Arc<Mutex<u32>>,
        admission_lock: Arc<Mutex<()>>,
    ) -> ExecutorConfig {
        ExecutorConfig {
            storage: storage.clone(),
            manager: manager.clone(),
            adapters: adapters.clone(),
            handle: Scheduler::start(system_clock()).0.handle,
            admission_lock,
            global: GlobalQuota::default(),
            running_global,
            shutdown_poll: StdDuration::from_millis(100),
        }
    }

    /// Regression: `admit_and_spawn_with_id` must DECREMENT
    /// `running_global` when `runs().create()` fails, otherwise a
    /// transient SQLite failure permanently inflates the global rail
    /// and the gate refuses every subsequent fire as
    /// `quota_global_max_concurrent_runs` until the daemon restarts.
    #[tokio::test]
    async fn admit_decrements_global_when_runs_create_fails() {
        let (_dir, storage, adapters, manager, cron) = fixture().await;
        let running_global = Arc::new(Mutex::new(0_u32));
        let admission_lock = Arc::new(Mutex::new(()));
        let cfg = exec_cfg(
            &storage,
            &adapters,
            &manager,
            running_global.clone(),
            admission_lock.clone(),
        );
        let now = Utc::now();
        let fire = FireEvent {
            cron_id: cron.id.clone(),
            scheduled_at: now,
            fired_at: now,
            coalesced_count: 1,
            catchup_degraded: false,
        };

        // Force a `runs().create()` failure by closing the storage
        // pools out from under the admission path. After close, the
        // INSERT will fail with `sqlx::Error::PoolClosed`; the contract
        // we are asserting is that the in-memory counter is rolled back
        // so the gate doesn't permanently refuse subsequent fires.
        storage.close().await;

        let err = admit_and_spawn_with_id(&cfg, &cron, &fire, None)
            .await
            .expect_err("storage failure must surface");
        assert!(matches!(err, AdmitError::Storage(_)), "got {err:?}");
        let after = *running_global.lock().await;
        assert_eq!(
            after, 0,
            "running_global must decrement on runs.create failure (leak rail otherwise stays at 1 forever)"
        );
    }

    /// Regression: the executor must NOT exit on the daemon-wide
    /// `daemon_shutdown` notifier that the accept loop fires during the
    /// connection-drain phase. The executor's exit signal lives inside
    /// `SchedulerServices` and is only fired by [`SchedulerServices::
    /// shutdown`] — strictly *after* the heap loop has been told to
    /// stop pushing fires.
    ///
    /// The pre-fix code shared one `Arc<Notify>` between the executor
    /// and the rest of the daemon, so any caller that triggered the
    /// shared notify would silently kill the executor mid-window and
    /// drop any fire the still-alive heap loop emitted afterwards.
    #[tokio::test]
    async fn executor_survives_daemon_shutdown_notify_until_services_shutdown() {
        let (_dir, storage, adapters, manager, _cron) = fixture().await;
        let daemon_shutdown = Arc::new(Notify::new());
        let services = SchedulerServices::start(
            storage.clone(),
            manager,
            adapters,
            SchedulerConfig::default(),
            daemon_shutdown.clone(),
        )
        .await
        .expect("scheduler boots");

        // Give the executor task ample time to enter its `select!` and
        // park on `shutdown.notified()` before we fire the notify — a
        // `notify_waiters()` call only wakes existing waiters, so
        // notifying too early loses the wake.
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        // Simulate the accept loop's `ctx.shutdown.notify_waiters()`.
        // After the fix this must NOT cause the executor to exit; with
        // the bug the executor would race to drain its empty channel
        // and `loops` would be effectively dead even though we never
        // called `services.shutdown()`.
        daemon_shutdown.notify_waiters();
        // Give the runtime ample time for the executor (if it were
        // listening on the daemon notify) to drain + exit.
        tokio::time::sleep(StdDuration::from_millis(300)).await;

        // Executor + heap must still be joinable (alive). We peek by
        // taking the lock — but DO NOT consume; just inspect.
        {
            let guard = services.loops.lock().await;
            let pair = guard.as_ref().expect("loops present pre-shutdown");
            assert!(
                !pair.0.is_finished(),
                "scheduler heap loop must outlive the daemon-wide notify"
            );
            assert!(
                !pair.1.is_finished(),
                "scheduler executor must outlive the daemon-wide notify — \
                 otherwise any fire emitted between the accept-loop notify and \
                 SchedulerServices::shutdown is silently dropped"
            );
        }

        // Now do the proper shutdown — both loops must wind down.
        services.shutdown().await;
    }

    /// Regression: a scheduled fire that the gate refuses or defers
    /// must STILL advance `crons.last_fired_at` / `next_fire_at`.
    /// Without this, a daemon restart re-walks the catch-up window from
    /// the stale watermark and may re-process the same tick — once the
    /// quota cap has space, the second pass spawns a real run (or, for
    /// audit-only refusals, writes a duplicate row). `crons.run_now`
    /// fires intentionally skip the watermark bump (out-of-band manual
    /// trigger should never consume a scheduled tick).
    #[tokio::test]
    async fn refused_scheduled_fire_advances_cron_watermark() {
        let (_dir, storage, adapters, manager, cron) = fixture().await;
        let running_global = Arc::new(Mutex::new(0_u32));
        let admission_lock = Arc::new(Mutex::new(()));
        let cfg = exec_cfg(
            &storage,
            &adapters,
            &manager,
            running_global.clone(),
            admission_lock.clone(),
        );

        // Provoke a global-cap refusal by pre-inflating the counter to
        // the cap. `GlobalQuota::default().global_max_concurrent_runs`
        // is 8 — set the counter to that so any new fire trips the
        // global rail.
        *running_global.lock().await = GlobalQuota::default().global_max_concurrent_runs;

        let now = Utc::now();
        let fire = FireEvent {
            cron_id: cron.id.clone(),
            scheduled_at: now,
            fired_at: now,
            coalesced_count: 1,
            catchup_degraded: false,
        };
        process_fire(&cfg, fire).await;

        let after = storage
            .crons()
            .get(&cron.id)
            .await
            .expect("query")
            .expect("cron present");
        assert!(
            after.last_fired_at.is_some(),
            "refused scheduled fire must still advance last_fired_at \
             (otherwise daemon restart re-catches-up the same tick)"
        );
        assert!(
            after.next_fire_at.is_some(),
            "refused scheduled fire must still advance next_fire_at"
        );
    }
}
