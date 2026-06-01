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
use std::time::Duration;

use la_adapter::AgentAdapter;
use la_core::{ManagerConfig, SessionManager};
use la_ipc::transport::{Endpoint, Listener};
use la_storage::{Storage, StorageConfig};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::dispatcher::{serve_connection, AdapterRegistry, ConnectionContext};
use crate::paths::{ensure_runtime_dir, SocketDiscovery, SocketLocation};
use crate::signals::DEFAULT_SHUTDOWN_DEADLINE;

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
        } = config;

        let socket = socket_discovery.resolve();
        ensure_runtime_dir(&socket.runtime_dir)?;
        ensure_socket_unbound(&socket.socket_path).await?;

        let database_path = state_dir.join("lad.sqlite");
        let storage_config = StorageConfig::new(database_path, state_dir.clone());
        let storage = Storage::open(storage_config).await?;

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

        let endpoint = endpoint_for(&socket.socket_path);
        let listener = Listener::bind(&endpoint).await?;

        let shutdown = Arc::new(Notify::new());
        let ctx = ConnectionContext {
            manager: manager.clone(),
            adapters: AdapterRegistry::from_map(adapters),
            server_version,
            shutdown: shutdown.clone(),
        };

        Ok(Self {
            manager,
            socket,
            listener,
            ctx,
            shutdown,
            shutdown_deadline,
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
