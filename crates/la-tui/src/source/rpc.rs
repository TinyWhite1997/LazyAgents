//! Daemon-backed [`SessionSource`] implementation.
//!
//! Owns a dedicated tokio current-thread runtime on a background OS
//! thread (mirrors [`crate::notif_sub`]) so the synchronous TUI event
//! loop can call [`SessionSource::snapshot`] without ever awaiting.
//!
//! Background-thread responsibilities:
//!
//! 1. Open one [`la_ipc::Connection`] to the daemon socket and run the
//!    standard [`la_ipc::client_handshake`].
//! 2. Issue `sessions.list` (with `include_archived = true` so the
//!    archived bucket is populated) on startup and then every
//!    [`POLL_INTERVAL`] (~2 s — A1's interim contract; A5 (WEK-95)
//!    will replace this with `sessions.changed` push notifications).
//! 3. Translate each [`SessionSummary`] into a [`SessionRow`] via
//!    [`SessionRow::from_summary`] and group them by `project_id` into
//!    [`ProjectGroup`]s. Project display metadata is derived from the
//!    session's `worktree_path` (basename + parent) because the daemon
//!    does not yet expose a `projects.list` RPC.
//! 4. Apply mutations posted from the main thread (`archive`, `delete`,
//!    `import_discovered`) by sending the matching RPC and then doing a
//!    fast re-poll so the next `snapshot()` reflects the change.
//!
//! A disconnected daemon never panics — `snapshot()` simply returns the
//! last good cache (possibly empty), and the bg loop reconnects with
//! exponential backoff just like [`crate::notif_sub::reconnect_loop`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use la_ipc::transport::{connect, endpoint_for};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request, ResponseOutcome};
use la_proto::methods::{
    AdaptersDiscover, AdaptersDiscoverParams, Method, SessionSummary, SessionsArchive,
    SessionsArchiveParams, SessionsCreate, SessionsCreateParams, SessionsCreateResult,
    SessionsDelete, SessionsDeleteParams, SessionsImport, SessionsImportParams, SessionsList,
    SessionsListParams,
};
use tokio::sync::mpsc;

use crate::model::{ProjectGroup, SessionRow};
use crate::source::{NewSessionRequest, ProjectId, SessionId, SessionSource, SourceError};

/// Background-loop tick. A1 contract: polling. A5 (WEK-95) replaces
/// this with `sessions.changed` push notifications.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Initial sleep after a connect/RPC failure. Mirrors [`crate::notif_sub`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Hard cap on how long [`SessionSource::create_session`] blocks the TUI
/// thread waiting for the bg loop's `sessions.create` round-trip. The
/// daemon's spawn path can stall on adapter probes, mutex contention,
/// or an unresponsive backend CLI while the socket still looks healthy
/// from our side; we'd rather close the modal with a Backend toast than
/// freeze the UI (review feedback from architect on PR #86 — `Esc`/`q`
/// must keep working). The user can retry from a fresh `n` press.
const CREATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Daemon-backed [`SessionSource`].
///
/// Construct with [`RpcSessionSource::connect`]. The constructor returns
/// immediately — the first snapshot may be empty until the background
/// thread has completed its initial `sessions.list` round-trip.
pub struct RpcSessionSource {
    cache: Arc<Mutex<Vec<ProjectGroup>>>,
    /// Locally-registered projects that the daemon's `sessions.list`
    /// snapshot has not yet surfaced (because the user has not spawned
    /// a session under them yet — M1's daemon only inserts a `projects`
    /// row lazily on the first `sessions.create`). Merged into the
    /// snapshot returned by [`SessionSource::snapshot`] so the sidebar
    /// shows the empty group immediately after a `create_project`
    /// call. Each entry is `(synthetic_id, root_path, display_name)`.
    /// Indexed by `root_path` so we can drop the local entry once the
    /// daemon-side row for the same path lands.
    pending_projects: Arc<Mutex<Vec<PendingProject>>>,
    /// Monotonic counter bumped by [`refresh_cache`] after every
    /// successful `sessions.list` round-trip. The runner snapshots this
    /// once per frame and dispatches [`crate::app::AppMsg::RefreshSessions`]
    /// when the value changes, so daemon-side mutations (poll tick,
    /// `sessions.create` follow-up refresh, A5's `sessions.changed` push
    /// once it lands) reach the sidebar within one frame instead of
    /// waiting for a user keystroke. Read-mostly, so `Relaxed` ordering
    /// is enough: we only need the value to eventually become visible
    /// to the TUI thread, not synchronise with anything else.
    refresh_gen: Arc<AtomicU64>,
    /// Async sender used to post mutations to the bg loop. Dropping the
    /// last sender (i.e. dropping `RpcSessionSource`) causes the bg
    /// loop's `recv()` to return `None`, which triggers a clean exit.
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// How long [`Self::create_session`] will block the TUI thread
    /// before giving up with [`SourceError::Backend`]. Tests override
    /// this via [`Self::set_create_timeout`] so the timeout arm can be
    /// exercised in < 1 s; production uses [`CREATE_TIMEOUT`].
    create_timeout: Duration,
    _thread: ThreadGuard,
}

/// Locally-pinned project that the daemon hasn't returned in
/// `sessions.list` yet (no sessions under it). Merged into snapshots so
/// the sidebar shows the empty group immediately after the user
/// registers the directory via [`SessionSource::create_project`].
#[derive(Debug, Clone)]
struct PendingProject {
    id: String,
    root_path: String,
    display_name: String,
}

/// Bg-thread command channel payload.
#[derive(Debug)]
enum Command {
    Archive(String),
    Delete(String),
    ImportDiscovered(String),
    /// Spawn a new session via `sessions.create`. The reply channel
    /// carries the daemon-assigned id (success) or the wire-level
    /// failure (mapped to [`SourceError::Backend`]) so the calling
    /// thread can block on a synchronous trait method. Uses
    /// [`std::sync::mpsc::SyncSender`] instead of `tokio::sync::oneshot`
    /// so the TUI thread can `recv_timeout` natively without dragging
    /// in an extra runtime handle.
    Create {
        req: NewSessionRequest,
        reply: std::sync::mpsc::SyncSender<Result<SessionId, SourceError>>,
    },
    /// Test-only: force a `sessions.list` refresh on the next loop turn.
    /// Reachable only through [`RpcSessionSource::force_refresh`] (also
    /// `#[doc(hidden)]`), which is the integration-test entry point.
    Refresh,
}

/// RAII guard whose drop releases the bg-thread join handle. We do not
/// block on join: a wedged bg thread should not stall TUI shutdown.
struct ThreadGuard {
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            drop(h);
        }
    }
}

impl RpcSessionSource {
    /// Spawn the background poller targeting `socket`. Returns immediately;
    /// the cache fills in once the bg loop completes its first
    /// `sessions.list`.
    pub fn connect(socket: &Path) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let cache = Arc::new(Mutex::new(Vec::<ProjectGroup>::new()));
        let pending_projects = Arc::new(Mutex::new(Vec::<PendingProject>::new()));
        let refresh_gen = Arc::new(AtomicU64::new(0));
        let socket = socket.to_path_buf();
        let cache_bg = cache.clone();
        let refresh_gen_bg = refresh_gen.clone();

        let handle = std::thread::Builder::new()
            .name("la-rpc-session-source".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        tracing::warn!(%err, "rpc-session-source: tokio runtime build failed");
                        return;
                    }
                };
                rt.block_on(async move {
                    reconnect_loop(socket, cache_bg, refresh_gen_bg, cmd_rx).await;
                });
            })
            .expect("spawn la-rpc-session-source thread");

        Self {
            cache,
            pending_projects,
            refresh_gen,
            cmd_tx,
            create_timeout: CREATE_TIMEOUT,
            _thread: ThreadGuard {
                handle: Some(handle),
            },
        }
    }

    /// Override [`CREATE_TIMEOUT`]. Public for the integration smoke
    /// tests in `tests/rpc_source_live.rs` that need to exercise the
    /// timeout arm without burning 10 s of wall-clock per run. Not
    /// intended for production callers — the constant is sized for
    /// real daemon latency, not unit tests.
    #[doc(hidden)]
    pub fn set_create_timeout(&mut self, d: Duration) {
        self.create_timeout = d;
    }

    /// Synchronously wait until the bg loop has populated the cache at
    /// least once (or `timeout` elapses). Returns whether the cache
    /// was observed non-empty. Test/diagnostic helper — production
    /// callers rely on the next snapshot pulling fresh data on its own.
    #[doc(hidden)]
    pub fn wait_for_first_snapshot(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let len = self.cache.lock().map(|c| c.len()).unwrap_or(0);
            if len > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Force a `sessions.list` refresh on the next loop turn. Used by
    /// the WEK-92-A4.1 integration tests (`tests/rpc_source_live.rs`)
    /// to exercise the bg-refresh signal without waiting the full
    /// [`POLL_INTERVAL`] of wall-clock — production callers should
    /// rely on the periodic poll (or A5's `sessions.changed` push once
    /// it lands) and not invoke this directly.
    #[doc(hidden)]
    pub fn force_refresh(&self) {
        let _ = self.cmd_tx.send(Command::Refresh);
    }
}

impl SessionSource for RpcSessionSource {
    fn snapshot(&self) -> Vec<ProjectGroup> {
        let mut groups = self.cache.lock().map(|c| c.clone()).unwrap_or_default();
        // Merge locally-registered "pending" projects (created via
        // `create_project` but not yet surfaced by the daemon's
        // `sessions.list` — the daemon only inserts a `projects` row on
        // first `sessions.create`). Drop pending entries whose
        // `root_path` (or display_name, which equals the basename used
        // by the daemon's `ensure_project`) now matches a daemon-side
        // group; that means the next session under the same path
        // promoted the local stub into a real row.
        if let Ok(mut pending) = self.pending_projects.lock() {
            pending.retain(|p| {
                !groups
                    .iter()
                    .any(|g| g.root_path == p.root_path || g.display_name == p.display_name)
            });
            for p in pending.iter() {
                // Insert before the synthetic Discovered / Archived
                // buckets so the regular project ordering is preserved.
                let insert_at = groups
                    .iter()
                    .position(|g| g.is_archived || g.is_discovered())
                    .unwrap_or(groups.len());
                let mut g = ProjectGroup::new(p.id.clone(), p.display_name.clone());
                g.root_path = p.root_path.clone();
                groups.insert(insert_at, g);
            }
        }
        groups
    }

    fn refresh_generation(&self) -> u64 {
        self.refresh_gen.load(Ordering::Relaxed)
    }

    fn archive(&mut self, session_id: &str) {
        let _ = self.cmd_tx.send(Command::Archive(session_id.to_string()));
    }

    fn delete(&mut self, session_id: &str) {
        let _ = self.cmd_tx.send(Command::Delete(session_id.to_string()));
    }

    fn restore(&mut self, _session_id: &str) {
        // The daemon does not currently expose a `sessions.restore` RPC
        // (no `SessionManager::restore` either — see
        // crates/la-core/src/manager.rs). The Sessions sidebar surfaces
        // a "restore from Archived" affordance against the mock today,
        // but until the daemon adds the API the live path silently
        // drops the request. WEK-95 (A5 sessions push subscription)
        // tracks the contract realignment; revisit then.
        tracing::debug!(
            session = %_session_id,
            "RpcSessionSource::restore is a no-op — daemon has no sessions.restore RPC yet"
        );
    }

    fn import_discovered(&mut self, session_id: &str) {
        let _ = self
            .cmd_tx
            .send(Command::ImportDiscovered(session_id.to_string()));
    }

    fn create_session(&mut self, req: NewSessionRequest) -> Result<SessionId, SourceError> {
        // Validation gate matches the mock: the daemon also rejects
        // these, but failing in the TUI keeps the round-trip out of the
        // hot path and lets the modal stay open with the user's input
        // intact (Validation arm).
        if req.backend.trim().is_empty() {
            return Err(SourceError::Validation("backend is required".into()));
        }
        if req.prompt.trim().is_empty() {
            return Err(SourceError::Validation("prompt cannot be empty".into()));
        }
        if req.project_dir.trim().is_empty() {
            return Err(SourceError::Validation(
                "project directory is missing — try a different project row".into(),
            ));
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.cmd_tx
            .send(Command::Create {
                req,
                reply: reply_tx,
            })
            .map_err(|_| {
                SourceError::Backend("rpc session source bg thread is no longer running".into())
            })?;
        // Block the TUI thread until the bg loop answers, but only up
        // to CREATE_TIMEOUT. A daemon that has the socket open but
        // sits on `sessions.create` (adapter probe stall, manager
        // mutex held, unresponsive backend CLI) would otherwise freeze
        // the UI — `Esc`/`q` wouldn't fire because the input loop is
        // parked here. Timing out lets the modal close with a Backend
        // toast so the user can decide to retry.
        match reply_rx.recv_timeout(self.create_timeout) {
            Ok(res) => res,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(SourceError::Backend(format!(
                "create timed out — daemon did not reply within {}s",
                self.create_timeout.as_secs()
            ))),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(SourceError::Backend(
                "rpc session source bg thread dropped the create reply".into(),
            )),
        }
    }

    fn create_project(&mut self, path: &str) -> Result<ProjectId, SourceError> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err(SourceError::Validation("path is required".into()));
        }
        let path_buf = std::path::Path::new(trimmed);
        if !path_buf.is_absolute() {
            return Err(SourceError::Validation(format!(
                "path must be absolute (was {trimmed:?})"
            )));
        }
        if !path_buf.is_dir() {
            return Err(SourceError::Validation(format!(
                "{trimmed} does not exist or is not a directory"
            )));
        }
        let display = path_buf
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| trimmed.to_string());
        // Duplicate guard: a path already in the live cache OR in the
        // pending list means the project is "already known" and we
        // refuse with a precise Validation error instead of stacking a
        // second stub. Matching on either `root_path` or `display_name`
        // mirrors the merge rule in `snapshot` — once the daemon adopts
        // this path it'll surface under the same display name (the
        // basename), and a stale stub would otherwise live forever
        // alongside the real entry.
        let live_has = self
            .cache
            .lock()
            .map(|c| {
                c.iter()
                    .any(|g| g.root_path == trimmed || g.display_name == display)
            })
            .unwrap_or(false);
        if live_has {
            return Err(SourceError::Validation(format!(
                "project already exists for {trimmed}"
            )));
        }
        let id = {
            let mut pending = self
                .pending_projects
                .lock()
                .map_err(|_| SourceError::Backend("pending_projects lock poisoned".into()))?;
            if pending
                .iter()
                .any(|p| p.root_path == trimmed || p.display_name == display)
            {
                return Err(SourceError::Validation(format!(
                    "project already exists for {trimmed}"
                )));
            }
            // Synthesise a UUID-looking id prefixed so it's distinguishable
            // from daemon-assigned ones in logs but interoperates with
            // the sidebar's plain-string id matching.
            let id = format!("pending-{}", pending.len() + 1);
            pending.push(PendingProject {
                id: id.clone(),
                root_path: trimmed.to_string(),
                display_name: display,
            });
            id
        };
        // Bump refresh_gen so the runner's per-frame
        // `refresh_generation()` check dispatches a RefreshSessions on
        // the very next loop, pulling the new stub into the sidebar
        // without waiting for the user to press a key.
        self.refresh_gen.fetch_add(1, Ordering::Relaxed);
        Ok(ProjectId(id))
    }
}

// ---------------------------------------------------------------------
// Background loop
// ---------------------------------------------------------------------

async fn reconnect_loop(
    socket: PathBuf,
    cache: Arc<Mutex<Vec<ProjectGroup>>>,
    refresh_gen: Arc<AtomicU64>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let mut connected = false;
        match run_once(&socket, &cache, &refresh_gen, &mut cmd_rx, &mut connected).await {
            Ok(()) => {
                // Sender side dropped — clean shutdown.
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "rpc-session-source: connection ended, reconnecting");
                if connected {
                    backoff = INITIAL_BACKOFF;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn run_once(
    socket: &Path,
    cache: &Arc<Mutex<Vec<ProjectGroup>>>,
    refresh_gen: &Arc<AtomicU64>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
    connected: &mut bool,
) -> Result<(), String> {
    let endpoint = endpoint_for(socket);
    let stream = tokio::time::timeout(Duration::from_secs(2), connect(&endpoint))
        .await
        .map_err(|_| format!("timed out connecting to {}", socket.display()))?
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let mut conn = Connection::new(stream);
    let _info = client_handshake(
        &mut conn,
        "la-rpc-session-source",
        env!("CARGO_PKG_VERSION"),
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .map_err(|e| format!("handshake: {e}"))?;
    *connected = true;

    // Initial pull so the first snapshot the App reads is populated.
    refresh_cache(&mut conn, cache, refresh_gen).await?;

    let mut next_id: i64 = 1;
    loop {
        // Block until either a command arrives or the poll deadline hits.
        let cmd = match tokio::time::timeout(POLL_INTERVAL, cmd_rx.recv()).await {
            // Tick fired without a command — periodic refresh.
            Err(_) => {
                refresh_cache(&mut conn, cache, refresh_gen).await?;
                continue;
            }
            // Sender dropped — clean exit.
            Ok(None) => return Ok(()),
            Ok(Some(c)) => c,
        };

        // Drain any extra commands that piled up so a burst doesn't
        // cause N back-to-back refreshes.
        let mut batch = vec![cmd];
        while let Ok(extra) = cmd_rx.try_recv() {
            batch.push(extra);
        }

        let mut mutated = false;
        for c in batch {
            match c {
                Command::Archive(id) => {
                    let req = Request::new(
                        next_id,
                        SessionsArchive::NAME,
                        SessionsArchiveParams { session_id: id },
                    )
                    .map_err(|e| format!("encode archive: {e}"))?;
                    next_id += 1;
                    send_and_await_ack(&mut conn, req).await?;
                    mutated = true;
                }
                Command::Delete(id) => {
                    let req = Request::new(
                        next_id,
                        SessionsDelete::NAME,
                        SessionsDeleteParams { session_id: id },
                    )
                    .map_err(|e| format!("encode delete: {e}"))?;
                    next_id += 1;
                    send_and_await_ack(&mut conn, req).await?;
                    mutated = true;
                }
                Command::ImportDiscovered(external_id) => {
                    // The SessionSource trait doesn't carry the
                    // backend identifier alongside the discovered row,
                    // so the bg loop has to look it up via
                    // adapters.discover. This is one extra RPC per
                    // import; acceptable because import is a manual `i`
                    // key press, not a hot path.
                    if let Some(backend) =
                        find_discovered_backend(&mut conn, &mut next_id, &external_id).await?
                    {
                        let req = Request::new(
                            next_id,
                            SessionsImport::NAME,
                            SessionsImportParams {
                                backend,
                                source_path: None,
                                external_ids: Some(vec![external_id]),
                            },
                        )
                        .map_err(|e| format!("encode import: {e}"))?;
                        next_id += 1;
                        send_and_await_ack(&mut conn, req).await?;
                        mutated = true;
                    } else {
                        tracing::debug!(
                            external = %external_id,
                            "import_discovered: external id not found in adapters.discover; dropping"
                        );
                    }
                }
                Command::Create {
                    req: new_req,
                    reply,
                } => {
                    // Translate trait-level fields to the wire shape.
                    // The TUI thread waits on the reply channel up to
                    // CREATE_TIMEOUT; we still notify the reply before
                    // bubbling a transport failure up to the reconnect
                    // loop so a wedged bg thread can't wedge the modal.
                    //
                    // `prompt` is always Some(...) here because
                    // [`SessionSource::create_session`] already rejected
                    // an empty buffer as `Validation`; the daemon also
                    // requires a non-empty prompt so we don't bother
                    // sending `None`.
                    let params = SessionsCreateParams {
                        project_dir: new_req.project_dir.clone(),
                        backend: new_req.backend.clone(),
                        args: new_req.args.clone(),
                        prompt: Some(new_req.prompt.clone()),
                        worktree: new_req.worktree,
                    };
                    let req = match Request::new(next_id, SessionsCreate::NAME, params) {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = reply
                                .send(Err(SourceError::Backend(format!("encode create: {e}"))));
                            continue;
                        }
                    };
                    next_id += 1;
                    match send_and_await_ack(&mut conn, req).await {
                        Ok(value) => match serde_json::from_value::<SessionsCreateResult>(value) {
                            Ok(r) => {
                                // create→snapshot read-your-write: pull
                                // a fresh `sessions.list` BEFORE acking
                                // the caller. The TUI thread unblocks on
                                // this reply and immediately calls
                                // `snapshot()`; if we ack first the
                                // sidebar shows the pre-create cache
                                // for at least one frame (and at worst
                                // until the next 2 s poll tick), making
                                // the new row look like it didn't land.
                                // Bubbling a refresh failure to the
                                // reconnect loop is fine: the caller
                                // still hears about it via the
                                // SourceError::Backend below, and a
                                // wedged conn would have died on the
                                // next op anyway.
                                if let Err(e) = refresh_cache(&mut conn, cache, refresh_gen).await {
                                    let _ = reply.send(Err(SourceError::Backend(format!(
                                        "create succeeded but follow-up refresh failed: {e}"
                                    ))));
                                    return Err(e);
                                }
                                let _ = reply.send(Ok(SessionId(r.session_id)));
                                // The cache is already current; skip the
                                // tail-of-loop refresh.
                            }
                            Err(e) => {
                                let _ = reply.send(Err(SourceError::Backend(format!(
                                    "decode create result: {e}"
                                ))));
                            }
                        },
                        Err(e) => {
                            // Tell the caller before bubbling the
                            // transport failure up to the reconnect
                            // loop — otherwise the TUI would wait the
                            // full CREATE_TIMEOUT for nothing.
                            let _ = reply.send(Err(SourceError::Backend(e.clone())));
                            return Err(e);
                        }
                    }
                }
                Command::Refresh => {
                    mutated = true;
                }
            }
        }

        if mutated {
            refresh_cache(&mut conn, cache, refresh_gen).await?;
        }
    }
}

/// Send `req` and wait until the matching `Response` arrives. Any
/// inbound notifications encountered in the meantime are dropped (the
/// `RpcSessionSource` does not subscribe to anything).
async fn send_and_await_ack(
    conn: &mut Connection<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>,
    req: Request,
) -> Result<serde_json::Value, String> {
    let expected_id = req.id.clone();
    conn.send(&Message::Request(req))
        .await
        .map_err(|e| format!("send request: {e}"))?;
    loop {
        let msg = conn
            .recv()
            .await
            .map_err(|e| format!("recv response: {e}"))?
            .ok_or_else(|| "daemon closed connection".to_string())?;
        match msg {
            Message::Response(resp) if resp.id == expected_id => match resp.outcome {
                ResponseOutcome::Result(v) => return Ok(v),
                ResponseOutcome::Error(e) => {
                    return Err(format!("daemon rejected request: {e}"));
                }
            },
            // Out-of-band notifications + unrelated responses are just
            // discarded — the source has no subscriptions.
            _ => continue,
        }
    }
}

/// Single `sessions.list` round-trip; overwrite the cache on success.
/// Bumps `refresh_gen` after a successful overwrite so the runner can
/// observe that a fresh snapshot is available and dispatch
/// [`crate::app::AppMsg::RefreshSessions`] on the next frame.
async fn refresh_cache(
    conn: &mut Connection<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>,
    cache: &Arc<Mutex<Vec<ProjectGroup>>>,
    refresh_gen: &Arc<AtomicU64>,
) -> Result<(), String> {
    let req = Request::new(
        // The id collides across refreshes but the daemon doesn't care
        // (no in-flight request multiplexing on this connection) and
        // we never read it back beyond matching the immediate ack.
        0i64,
        SessionsList::NAME,
        SessionsListParams {
            project: None,
            backend: None,
            include_archived: true,
        },
    )
    .map_err(|e| format!("encode list: {e}"))?;
    let value = send_and_await_ack(conn, req).await?;
    let result: la_proto::methods::SessionsListResult =
        serde_json::from_value(value).map_err(|e| format!("decode list result: {e}"))?;
    let groups = build_groups(&result.sessions);
    if let Ok(mut guard) = cache.lock() {
        *guard = groups;
    }
    // Order the bump AFTER the cache write so a runner reading
    // `refresh_generation()` and then `snapshot()` sees the same view
    // the daemon just returned. Relaxed is fine: the runner's
    // try_recv-style loop tolerates a one-frame visibility delay, and
    // the cache write itself is mutex-synchronised.
    refresh_gen.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Call `adapters.discover` and return the backend that owns
/// `external_id`, if any.
async fn find_discovered_backend(
    conn: &mut Connection<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>,
    next_id: &mut i64,
    external_id: &str,
) -> Result<Option<String>, String> {
    let req = Request::new(
        *next_id,
        AdaptersDiscover::NAME,
        AdaptersDiscoverParams::default(),
    )
    .map_err(|e| format!("encode discover: {e}"))?;
    *next_id += 1;
    let value = send_and_await_ack(conn, req).await?;
    let result: la_proto::methods::AdaptersDiscoverResult =
        serde_json::from_value(value).map_err(|e| format!("decode discover: {e}"))?;
    for d in result.discovered {
        if d.external_id == external_id {
            return Ok(Some(d.backend));
        }
    }
    Ok(None)
}

/// Convert a flat `sessions.list` payload into the grouped, ordered
/// shape the sidebar expects. The Archived bucket goes last; the
/// project ordering is by display name (stable across snapshots).
fn build_groups(sessions: &[SessionSummary]) -> Vec<ProjectGroup> {
    use la_proto::methods::SessionState;

    struct PendingGroup {
        display_name: String,
        root_path: String,
        sessions: Vec<SessionRow>,
    }

    let mut by_project: BTreeMap<String, PendingGroup> = BTreeMap::new();
    let mut archived = ProjectGroup::archived();

    for s in sessions {
        let row = SessionRow::from_summary(s);
        if matches!(s.state, SessionState::Archived) {
            archived.sessions.push(row);
            continue;
        }
        let entry = by_project
            .entry(s.project_id.clone())
            .or_insert_with(|| PendingGroup {
                display_name: derive_display_name(&s.project_id, s.worktree_path.as_deref()),
                root_path: derive_root_path(s.worktree_path.as_deref()),
                sessions: Vec::new(),
            });
        // Fill blanks the first session lacked.
        if entry.display_name.is_empty() {
            entry.display_name = derive_display_name(&s.project_id, s.worktree_path.as_deref());
        }
        if entry.root_path.is_empty() {
            entry.root_path = derive_root_path(s.worktree_path.as_deref());
        }
        entry.sessions.push(row);
    }

    let mut groups: Vec<ProjectGroup> = by_project
        .into_iter()
        .map(|(project_id, pg)| {
            let mut g = ProjectGroup::new(project_id, pg.display_name);
            g.root_path = pg.root_path;
            g.sessions = pg.sessions;
            g
        })
        .collect();
    if !archived.sessions.is_empty() {
        groups.push(archived);
    }
    groups
}

/// Best-effort project display name derived from the worktree path.
/// Falls back to a short project-id prefix when there is no worktree
/// path (sessions sharing the project root).
fn derive_display_name(project_id: &str, worktree_path: Option<&str>) -> String {
    if let Some(p) = worktree_path {
        if let Some(name) = Path::new(p).file_name().and_then(|n| n.to_str()) {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    let head: String = project_id.chars().take(8).collect();
    if head.is_empty() {
        "(no project)".to_string()
    } else {
        head
    }
}

/// Best-effort root path: the worktree path itself, or empty.
fn derive_root_path(worktree_path: Option<&str>) -> String {
    worktree_path.unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use la_proto::methods::SessionState;

    fn summary(
        id: &str,
        project: &str,
        backend: &str,
        state: SessionState,
        worktree: Option<&str>,
    ) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            project_id: project.into(),
            backend: backend.into(),
            title: None,
            state,
            origin: "user".into(),
            created_at: "2026-06-06T00:00:00Z".into(),
            updated_at: "2026-06-06T00:00:00Z".into(),
            worktree_path: worktree.map(str::to_string),
        }
    }

    #[test]
    fn build_groups_pins_archived_last_and_derives_display_name() {
        let sessions = vec![
            summary(
                "s1",
                "proj-a",
                "claude",
                SessionState::Running,
                Some("/home/me/code/proj-a"),
            ),
            summary("s2", "proj-a", "codex", SessionState::Exited, None),
            summary(
                "s3",
                "proj-b",
                "opencode",
                SessionState::Archived,
                Some("/tmp/proj-b"),
            ),
        ];
        let groups = build_groups(&sessions);
        assert_eq!(groups.len(), 2);
        assert!(!groups[0].is_archived);
        assert_eq!(groups[0].display_name, "proj-a");
        assert_eq!(groups[0].sessions.len(), 2);
        let archived = groups.last().unwrap();
        assert!(archived.is_archived);
        assert_eq!(archived.sessions.len(), 1);
        assert_eq!(archived.sessions[0].session_id, "s3");
    }

    #[test]
    fn build_groups_falls_back_to_project_id_when_no_worktree() {
        let sessions = vec![summary(
            "s1",
            "01934fff-feed-7000-a000-aaaaaaaaa001",
            "claude",
            SessionState::Exited,
            None,
        )];
        let groups = build_groups(&sessions);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].display_name, "01934fff");
    }

    #[test]
    fn build_groups_omits_archived_bucket_when_empty() {
        let sessions = vec![summary("s1", "p1", "claude", SessionState::Exited, None)];
        let groups = build_groups(&sessions);
        assert_eq!(groups.len(), 1);
        assert!(!groups[0].is_archived);
    }
}

// Re-exported for the integration test in `tests/rpc_source_live.rs`.
