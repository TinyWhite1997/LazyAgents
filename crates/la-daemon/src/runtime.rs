//! Daemon assembly: storage + adapter registry + listener glue.
//!
//! [`Daemon`] is the in-process façade used by both the `lad` binary and
//! integration tests. It opens [`la_storage::Storage`], builds a
//! [`la_core::SessionManager`], registers adapters, binds the UDS
//! listener, and runs `accept` → `serve_connection` until either the
//! caller drops the [`DaemonHandle`] or a SIGINT/SIGTERM fires.
//!
//! Architecture §2.1 invariant: this is the **only** place that wires
//! la-core to la-ipc + la-storage. Test code can override every knob via
//! [`DaemonConfig`] so the same code path runs in production and in the
//! integration suite.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use la_adapter::AgentAdapter;
use la_core::{ManagerConfig, SessionManager, WorktreeManager};
use la_ipc::transport::{Endpoint, Listener};
use la_storage::{Storage, StorageConfig};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::dispatcher::{serve_connection, AdapterRegistry, ConnectionContext};
use crate::health::{spawn_loop, HealthRegistry, ProbeLoopConfig, DEFAULT_PROBE_INTERVAL};
use crate::paths::{ensure_runtime_dir, SocketDiscovery, SocketLocation};
use crate::signals::DEFAULT_SHUTDOWN_DEADLINE;

/// WEK-27 TTL for archived worktree sweep: rows whose `archived_at`
/// is older than this are reaped on daemon startup and on each
/// [`WORKTREE_SWEEP_INTERVAL`] tick. Issue body pins 7 days.
pub const WORKTREE_SWEEP_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How often the daemon re-runs `WorktreeManager::sweep_expired`. Once
/// an hour comfortably misses the 7-day TTL by several orders of
/// magnitude, but keeps the loop responsive enough that a long-running
/// daemon doesn't accumulate weeks of orphan worktrees.
pub const WORKTREE_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub const RUNS_ARCHIVE_RETENTION_DAYS: i64 = 90;
pub const RUNS_ARCHIVE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const RUNS_ARCHIVE_UTC_SECONDS: u64 = 3 * 60 * 60 + 17 * 60;

/// Errors surfaced while spinning up a daemon.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage: {0}")]
    Storage(#[from] la_storage::StorageError),
    #[error("ipc: {0}")]
    Ipc(#[from] la_ipc::IpcError),
    #[error("a daemon is already listening on {0}")]
    AlreadyRunning(PathBuf),
}

/// Everything the daemon needs to know up front. All fields have
/// reasonable defaults that match the production path; tests typically
/// override `socket_discovery` and `state_dir` to point at tempdirs.
pub struct DaemonConfig {
    /// Where the SQLite + spilled transcripts live.
    pub state_dir: PathBuf,
    /// Override for the socket file. Defaults to per-version discovery.
    pub socket_discovery: SocketDiscovery,
    /// Adapter registry. Keep the field public so callers can register
    /// custom adapters (mock CLIs in tests, future plugins in prod).
    pub adapters: HashMap<String, Arc<dyn AgentAdapter>>,
    /// Tunables forwarded to [`SessionManager::new`].
    pub manager: ManagerConfig,
    /// Server-version string baked into the handshake response. Defaults
    /// to [`crate::SERVER_VERSION`].
    pub server_version: String,
    /// Hard cap on the graceful shutdown sequence (architecture §6.4).
    pub shutdown_deadline: Duration,
    /// How often the daemon re-probes each adapter and re-broadcasts
    /// `daemon.health` (WEK-29 / M2.6). Tests typically shrink this so
    /// they don't have to wait the full 60 s between rounds.
    pub probe_interval: Duration,
    /// WEK-27: TTL for the archived-worktree sweep loop. Defaults to
    /// [`WORKTREE_SWEEP_TTL`] (7 days). Tests shrink it to a few ms so
    /// the sweep predicate can be asserted without sleeping a week.
    pub worktree_sweep_ttl: Duration,
    /// WEK-27: how often the sweep loop wakes up. Defaults to
    /// [`WORKTREE_SWEEP_INTERVAL`] (1 hour). Tests shrink this together
    /// with `worktree_sweep_ttl` to drive the periodic path.
    pub worktree_sweep_interval: Duration,
    /// Retention window for `runs` rows before they are compressed to
    /// `runs/archive/<yyyymm>.jsonl.zst` and deleted from SQLite.
    pub runs_archive_retention_days: i64,
    /// Production leaves this `None`, which means "next 03:17 UTC".
    /// Tests can set `Some(Duration::ZERO)` and use Tokio's paused time
    /// to prove the day-counter path fires without sleeping.
    pub runs_archive_initial_delay: Option<Duration>,
    pub runs_archive_interval: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            state_dir: crate::paths::default_state_dir(),
            socket_discovery: SocketDiscovery::default(),
            adapters: HashMap::new(),
            manager: ManagerConfig::default(),
            server_version: crate::SERVER_VERSION.to_string(),
            shutdown_deadline: DEFAULT_SHUTDOWN_DEADLINE,
            probe_interval: DEFAULT_PROBE_INTERVAL,
            worktree_sweep_ttl: WORKTREE_SWEEP_TTL,
            worktree_sweep_interval: WORKTREE_SWEEP_INTERVAL,
            runs_archive_retention_days: RUNS_ARCHIVE_RETENTION_DAYS,
            runs_archive_initial_delay: None,
            runs_archive_interval: RUNS_ARCHIVE_INTERVAL,
        }
    }
}

/// Live daemon. The [`accept_loop`](Self::accept_loop) future runs until
/// [`DaemonHandle::shutdown`] is called or a SIGINT/SIGTERM is observed.
pub struct Daemon {
    pub manager: SessionManager,
    pub socket: SocketLocation,
    listener: Listener,
    ctx: ConnectionContext,
    shutdown: Arc<Notify>,
    shutdown_deadline: Duration,
    /// Health probe loop join handle — awaited on graceful shutdown so
    /// the last SQLite upsert isn't truncated mid-write.
    health_loop: Option<JoinHandle<()>>,
    /// WEK-27 archived-worktree sweep loop. Same shutdown discipline
    /// as `health_loop`: aborted on graceful shutdown so the daemon
    /// doesn't tear the SQLite handle out from under an in-flight
    /// query.
    worktree_sweep_loop: Option<JoinHandle<()>>,
    runs_archive_loop: Option<JoinHandle<()>>,
}

impl Daemon {
    /// Open storage, bind the socket, register adapters. Does NOT enter
    /// the accept loop — call [`accept_loop`](Self::accept_loop) (or use
    /// [`spawn`](Self::spawn) for a tokio task) to start serving.
    pub async fn bind(config: DaemonConfig) -> Result<Self, DaemonError> {
        let DaemonConfig {
            state_dir,
            socket_discovery,
            adapters,
            manager: manager_config,
            server_version,
            shutdown_deadline,
            probe_interval,
            worktree_sweep_ttl,
            worktree_sweep_interval,
            runs_archive_retention_days,
            runs_archive_initial_delay,
            runs_archive_interval,
        } = config;

        let socket = socket_discovery.resolve();
        ensure_runtime_dir(&socket.runtime_dir)?;
        ensure_socket_unbound(&socket.socket_path).await?;

        let database_path = state_dir.join("lad.sqlite");
        let storage_config = StorageConfig::new(database_path, state_dir.clone());
        let storage = Storage::open(storage_config).await?;

        // WEK-27: provision the per-session worktree manager once at
        // startup. We always construct it (the directory is created
        // lazily on first use) and stash the Arc on `ManagerConfig` so
        // `SessionManager::spawn_with_options` can honour
        // `worktree=true` without a follow-up wiring step. Capability
        // bit flips on the same path so the client knows the daemon
        // will actually act on the flag.
        let worktree_mgr = Arc::new(WorktreeManager::for_state_dir(&state_dir));
        let mut manager_config = manager_config;
        manager_config.worktree = Some(worktree_mgr.clone());

        // Refresh the backends table from the live adapter set so
        // `sessions.list` joins still resolve even on a fresh install.
        // The registry key is what the wire surface uses (clients pass it
        // as `SessionsCreateParams.backend`); we assert it matches the
        // adapter's own declared id to catch mis-registration early.
        for (id, adapter) in &adapters {
            let desc = adapter.descriptor();
            debug_assert_eq!(
                id.as_str(),
                desc.id,
                "adapter registered as {id:?} declares descriptor id {:?}",
                desc.id
            );
            storage
                .backends()
                .upsert(la_storage::BackendUpsert {
                    id,
                    display_name: desc.display_name,
                    version: None,
                    available: true,
                })
                .await?;
        }

        let manager = SessionManager::new(storage.clone(), manager_config);
        let reaped = manager.reap_orphans().await.unwrap_or(0);
        if reaped > 0 {
            tracing::info!(count = reaped, "reaped orphan sessions on startup");
        }

        // WEK-27 §2.4: best-effort `git worktree prune` per known
        // project root on startup so the daemon picks up after a
        // crashed predecessor that left orphan worktree entries in
        // `<repo>/.git/worktrees/`. The call is non-fatal — a wedged
        // repo logs and the daemon still boots.
        if let Ok(projects) = storage.projects().list().await {
            for p in projects {
                worktree_mgr
                    .prune_orphans(std::path::Path::new(&p.root_path))
                    .await;
            }
        }

        // WEK-27 §2.4 acceptance: 7-day TTL sweep of archived
        // worktrees. Runs once on startup so a daemon that's been
        // down past the TTL still catches up, then on a tick by the
        // background loop below.
        let (sweep_ok, sweep_err) = worktree_mgr
            .sweep_expired(&storage, worktree_sweep_ttl)
            .await;
        if sweep_ok > 0 || sweep_err > 0 {
            tracing::info!(
                swept = sweep_ok,
                failed = sweep_err,
                ttl_secs = worktree_sweep_ttl.as_secs(),
                "worktree sweep on startup"
            );
        }

        let endpoint = endpoint_for(&socket.socket_path);
        let listener = Listener::bind(&endpoint).await?;

        let shutdown = Arc::new(Notify::new());
        let registry = AdapterRegistry::from_map(adapters);
        let health_registry = HealthRegistry::new();
        let ctx = ConnectionContext {
            manager: manager.clone(),
            adapters: registry.clone(),
            health: health_registry.clone(),
            server_version,
            shutdown: shutdown.clone(),
        };

        // Spawn the WEK-29 probe + broadcast loop. Holds clones of
        // every component (registry / storage / bus / manager handle)
        // and is awaited on graceful shutdown so the last upsert
        // lands before the SQLite handle closes.
        let probe_cfg = ProbeLoopConfig {
            adapters: registry.pairs(),
            registry: health_registry,
            storage: manager.storage().clone(),
            bus: manager.bus(),
            manager: manager.clone(),
            interval: probe_interval,
            shutdown: shutdown.clone(),
        };
        let health_loop = Some(spawn_loop(probe_cfg));

        // WEK-27 background sweep tick. Cheap (one indexed SELECT per
        // tick when the workload is at rest), so the interval doesn't
        // need to be tuned per-deployment.
        let sweep_loop = Some(spawn_worktree_sweep_loop(WorktreeSweepLoopConfig {
            worktree: worktree_mgr.clone(),
            storage: manager.storage().clone(),
            ttl: worktree_sweep_ttl,
            interval: worktree_sweep_interval,
            shutdown: shutdown.clone(),
        }));

        let runs_archive_loop = Some(spawn_runs_archive_loop(RunsArchiveLoopConfig {
            storage: manager.storage().clone(),
            retention_days: runs_archive_retention_days,
            initial_delay: runs_archive_initial_delay
                .unwrap_or_else(delay_until_next_runs_archive_tick),
            interval: runs_archive_interval,
            shutdown: shutdown.clone(),
        }));

        Ok(Self {
            manager,
            socket,
            listener,
            ctx,
            shutdown,
            shutdown_deadline,
            health_loop,
            worktree_sweep_loop: sweep_loop,
            runs_archive_loop,
        })
    }

    /// Convenience: produce a [`DaemonHandle`] you can use to ask the
    /// daemon to wind down. Cheap (single `Notify` clone); call before
    /// dropping the daemon into a `tokio::spawn`.
    pub fn handle(&self) -> DaemonHandle {
        DaemonHandle {
            shutdown: self.shutdown.clone(),
        }
    }

    /// Spawn the accept loop on the current tokio runtime and return a
    /// [`JoinHandle`]. The handle joins once the loop has wound down and
    /// every in-flight connection has finished its teardown.
    pub fn spawn(self) -> (DaemonHandle, JoinHandle<()>) {
        let handle = self.handle();
        let join = tokio::spawn(async move {
            self.accept_loop().await;
        });
        (handle, join)
    }

    /// Run the accept loop in the current task. Returns once
    /// [`DaemonHandle::shutdown`] is called or the shutdown deadline
    /// elapses after we stopped accepting new connections.
    pub async fn accept_loop(self) {
        let Daemon {
            manager,
            socket,
            listener,
            ctx,
            shutdown,
            shutdown_deadline,
            health_loop,
            worktree_sweep_loop,
            runs_archive_loop,
        } = self;

        tracing::info!(socket = %socket.socket_path.display(), "lad listening");

        let mut conns: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    tracing::info!("shutdown notified — stopping accept loop");
                    break;
                }
                accept = listener.accept() => {
                    match accept {
                        Ok(stream) => {
                            let ctx = ctx.clone();
                            conns.spawn(async move {
                                serve_connection(stream, ctx).await;
                            });
                        }
                        Err(err) => {
                            tracing::warn!(%err, "accept failed");
                            // brief backoff to avoid a hot loop on a
                            // persistent error (e.g. EMFILE)
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
                Some(_finished) = conns.join_next() => {}
            }
        }

        // Tell connections to wind down; they observe `ctx.shutdown.notified()`.
        ctx.shutdown.notify_waiters();

        // Stop the non-critical background loops *before* we start the
        // §6.4 10 s countdown. Their `.await` could otherwise add real
        // wall time on a CI box mid-tick (a sweep iteration that's
        // chained 2–3 git invocations can take ~1 s before reaching
        // an abort-point), and §6.4's shutdown ceiling applies only to
        // the SIGTERM → SIGKILL escalation — not to bookkeeping tasks.
        // Each `.await` is bounded so a pathologically slow unwind
        // never blocks shutdown either.
        const BACKGROUND_LOOP_JOIN_BUDGET: Duration = Duration::from_millis(200);
        if let Some(h) = health_loop {
            h.abort();
            let _ = tokio::time::timeout(BACKGROUND_LOOP_JOIN_BUDGET, h).await;
        }
        if let Some(h) = worktree_sweep_loop {
            h.abort();
            let _ = tokio::time::timeout(BACKGROUND_LOOP_JOIN_BUDGET, h).await;
        }
        if let Some(h) = runs_archive_loop {
            h.abort();
            let _ = tokio::time::timeout(BACKGROUND_LOOP_JOIN_BUDGET, h).await;
        }

        // Best-effort: SIGTERM live sessions so PTY children get a chance
        // to clean up. The session manager's per-session pump observes
        // child exit and persists state; we just initiate the request.
        let ids = manager.active_ids().await;
        for id in ids {
            if let Err(err) = manager
                .signal(&id, la_proto::methods::SessionSignal::Term)
                .await
            {
                tracing::debug!(%err, session = %id.as_str(), "shutdown signal failed");
            }
        }

        // §6.4: "整个序列在 daemon 关闭时对所有 session 并发执行，硬超时
        // 10 s". `hard_deadline` is the single ceiling that every drain
        // phase below honours; the per-phase budgets are carved out of it
        // so connection drain + SIGTERM grace + SIGKILL all complete
        // before it expires.
        //
        // Phase budget inside `shutdown_deadline`:
        //   - first half  → connection drain (writer flush, sub teardown)
        //   - second half → SIGTERM grace before escalating to SIGKILL
        // The numbers are advisory; the only invariant the contract cares
        // about is that the SIGKILL path runs strictly before
        // `hard_deadline`.
        let hard_deadline = tokio::time::Instant::now() + shutdown_deadline;
        let term_grace_deadline = tokio::time::Instant::now() + shutdown_deadline / 2;

        // Drain in-flight connection tasks until either they all finish
        // or we hit the SIGTERM-grace milestone — leaving the rest of
        // the budget for the kill escalation below.
        while !conns.is_empty() {
            let now = tokio::time::Instant::now();
            if now >= term_grace_deadline {
                tracing::warn!(
                    pending = conns.len(),
                    "connection drain budget exhausted — aborting remaining connections"
                );
                conns.abort_all();
                break;
            }
            let remaining = term_grace_deadline.saturating_duration_since(now);
            match tokio::time::timeout(remaining, conns.join_next()).await {
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        // Wait for live sessions to actually exit (their state pump
        // closes the storage row and drops the registry entry on its
        // own when the child exits). If anything is still around at
        // `hard_deadline - epsilon`, escalate to SIGKILL so the
        // §6.4 10 s ceiling is observed.
        const KILL_LANDING_TICK: Duration = Duration::from_millis(200);
        let kill_by = hard_deadline.saturating_duration_since(tokio::time::Instant::now());
        let kill_escalation_at =
            tokio::time::Instant::now() + kill_by.saturating_sub(KILL_LANDING_TICK);
        while manager.active_count().await > 0 {
            let now = tokio::time::Instant::now();
            if now >= kill_escalation_at {
                let remaining = manager.active_ids().await;
                tracing::warn!(
                    pending = remaining.len(),
                    "graceful deadline elapsed; escalating to SIGKILL"
                );
                for id in remaining {
                    let _ = manager
                        .signal(&id, la_proto::methods::SessionSignal::Kill)
                        .await;
                }
                // Brief tick so the kill can land + the pump can persist
                // the exited row — bounded by `hard_deadline`.
                let landing = hard_deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(KILL_LANDING_TICK);
                if !landing.is_zero() {
                    tokio::time::sleep(landing).await;
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Health probe loop + worktree sweep loop were already torn
        // down at the top of the shutdown sequence (before the §6.4
        // 10 s countdown started) so they don't eat into the SIGKILL
        // budget. Nothing more to do here besides closing storage.

        manager.storage().close().await;
        // Drop the listener to remove the socket file; the la-ipc Listener
        // has a Drop impl that unlinks the path if it still owns it.
        drop(listener);
    }
}

/// Cheap shutdown trigger. Cloneable; clones share the same `Notify` so
/// every holder can ask the daemon to wind down.
#[derive(Clone)]
pub struct DaemonHandle {
    shutdown: Arc<Notify>,
}

impl DaemonHandle {
    /// Ask the daemon to stop accepting new connections and begin the
    /// graceful shutdown sequence. Non-blocking — the accept loop's
    /// `JoinHandle` reports completion.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}

fn endpoint_for(path: &Path) -> Endpoint {
    #[cfg(unix)]
    {
        Endpoint::uds(path)
    }
    #[cfg(not(unix))]
    {
        let pipe_name = format!(
            r"\\.\pipe\lazyagents-{}",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("lad")
        );
        Endpoint::named_pipe(pipe_name)
    }
}

/// Refuse to start if a daemon is already listening on the same path.
///
/// We attempt a connect on Unix: if it succeeds we assume someone else
/// is alive. If we get `ECONNREFUSED` the socket file is a leftover from
/// a crashed run and `Listener::bind` will clean it up. Other errors
/// (`ENOENT`, etc.) just mean nothing is there.
async fn ensure_socket_unbound(path: &Path) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        if !path.exists() {
            return Ok(());
        }
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => Err(DaemonError::AlreadyRunning(path.to_path_buf())),
            Err(_) => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Knobs for [`spawn_worktree_sweep_loop`]. Kept as its own struct so
/// integration tests can shrink the interval / TTL without re-wiring
/// the whole daemon.
struct WorktreeSweepLoopConfig {
    worktree: Arc<WorktreeManager>,
    storage: la_storage::Storage,
    ttl: Duration,
    interval: Duration,
    shutdown: Arc<Notify>,
}

/// Spawn a background task that wakes every `interval` and asks the
/// [`WorktreeManager`] to reap archived rows older than `ttl`. Mirrors
/// the health-loop pattern: every step is a single SQLite read + a
/// bounded number of git invocations, so abort-at-an-`.await`-point
/// during shutdown is graceful enough.
fn spawn_worktree_sweep_loop(cfg: WorktreeSweepLoopConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        let WorktreeSweepLoopConfig {
            worktree,
            storage,
            ttl,
            interval,
            shutdown,
        } = cfg;
        loop {
            tokio::select! {
                _ = shutdown.notified() => break,
                _ = tokio::time::sleep(interval) => {
                    let (ok, err) = worktree.sweep_expired(&storage, ttl).await;
                    if ok > 0 || err > 0 {
                        tracing::info!(
                            swept = ok,
                            failed = err,
                            ttl_secs = ttl.as_secs(),
                            "worktree sweep tick"
                        );
                    }
                }
            }
        }
    })
}

struct RunsArchiveLoopConfig {
    storage: la_storage::Storage,
    retention_days: i64,
    initial_delay: Duration,
    interval: Duration,
    shutdown: Arc<Notify>,
}

fn spawn_runs_archive_loop(cfg: RunsArchiveLoopConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        let RunsArchiveLoopConfig {
            storage,
            retention_days,
            initial_delay,
            interval,
            shutdown,
        } = cfg;

        let mut sleep = Box::pin(tokio::time::sleep(initial_delay));
        loop {
            tokio::select! {
                _ = shutdown.notified() => break,
                _ = &mut sleep => {
                    match storage.runs().archive_older_than_days(retention_days).await {
                        Ok(outcome) if outcome.archived_rows > 0 => {
                            tracing::info!(
                                rows = outcome.archived_rows,
                                files = outcome.archive_files,
                                retention_days,
                                "runs archive tick"
                            );
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(%err, retention_days, "runs archive tick failed");
                        }
                    }
                    sleep.as_mut().reset(tokio::time::Instant::now() + interval);
                }
            }
        }
    })
}

fn delay_until_next_runs_archive_tick() -> Duration {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let day_pos = now_secs % RUNS_ARCHIVE_INTERVAL.as_secs();
    let wait_secs = if day_pos < RUNS_ARCHIVE_UTC_SECONDS {
        RUNS_ARCHIVE_UTC_SECONDS - day_pos
    } else {
        RUNS_ARCHIVE_INTERVAL.as_secs() - day_pos + RUNS_ARCHIVE_UTC_SECONDS
    };
    Duration::from_secs(wait_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use la_storage::{BackendUpsert, CronUpsert, NewProject, NewRun, Storage, StorageConfig};
    use tempfile::TempDir;

    async fn archive_loop_storage() -> (TempDir, Storage, Arc<Notify>) {
        let dir = TempDir::new().expect("tempdir");
        let storage = Storage::open(StorageConfig::for_test(dir.path()))
            .await
            .expect("open storage");
        storage
            .backends()
            .upsert(BackendUpsert {
                id: "claude",
                display_name: "Claude Code",
                version: None,
                available: true,
            })
            .await
            .expect("backend");
        storage
            .projects()
            .create(NewProject {
                id: "project-1".into(),
                root_path: "/tmp/lazyagents/archive-loop".into(),
                display_name: "archive-loop".into(),
                vcs: Some("git".into()),
            })
            .await
            .expect("project");
        storage
            .crons()
            .upsert(CronUpsert {
                id: "cron-1".into(),
                name: "daily".into(),
                enabled: true,
                project_id: "project-1".into(),
                backend_id: "claude".into(),
                spawn_args: serde_json::json!({}),
                prompt: "status".into(),
                cron_expr: "17 3 * * *".into(),
                tz: "UTC".into(),
                catchup_mode: "coalesce".into(),
                max_concurrent_runs: 1,
                max_runs_per_day: 24,
                max_runtime_s: 1800,
                cost_budget_usd_per_day: None,
                failure_backoff: "expo(1m,2,1h)".into(),
                pause_on_consecutive_failures: 5,
                consecutive_failures: 0,
                last_fired_at: None,
                next_fire_at: Some("2026-01-01 03:17:00".into()),
            })
            .await
            .expect("cron");
        storage
            .runs()
            .create(NewRun {
                id: "run-old".into(),
                cron_id: Some("cron-1".into()),
                session_id: None,
                scheduled_at: "2000-01-01 03:17:00".into(),
                started_at: None,
                status: "pending".into(),
                coalesced_count: 1,
            })
            .await
            .expect("run");
        (dir, storage, Arc::new(Notify::new()))
    }

    #[tokio::test]
    async fn runs_archive_loop_fires_under_paused_time() {
        let (_dir, storage, shutdown) = archive_loop_storage().await;
        tokio::time::pause();
        let handle = spawn_runs_archive_loop(RunsArchiveLoopConfig {
            storage: storage.clone(),
            retention_days: 90,
            initial_delay: Duration::from_secs(24 * 60 * 60),
            interval: Duration::from_secs(24 * 60 * 60),
            shutdown: shutdown.clone(),
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
        tokio::time::resume();
        for _ in 0..20 {
            if storage.runs().get("run-old").await.unwrap().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(storage.runs().get("run-old").await.unwrap().is_none());
        assert!(storage
            .data_dir()
            .join("runs/archive/200001.jsonl.zst")
            .exists());

        shutdown.notify_waiters();
        handle.await.expect("archive loop joins");
    }
}
