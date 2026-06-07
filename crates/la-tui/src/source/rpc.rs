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
///
/// Two eviction keys, applied in [`SessionSource::snapshot`] (both must
/// match the same daemon group to drop the stub):
///
/// - `root_path` — works when the daemon-side group carries a non-empty
///   `root_path` (i.e. the project's first session was a worktree
///   session and `SessionSummary.worktree_path` was populated).
/// - `daemon_project_id` — populated after the bg loop sees a
///   `sessions.create` reply for this stub's directory and looks up the
///   newly-spawned session's `project_id` in the refreshed cache. This
///   is what evicts the stub for the **non-worktree** first-session
///   case, where `sessions.list` returns `worktree_path: None` and
///   `build_groups` derives `root_path = ""` (Code Reviewer's blocker
///   on PR #92 round 2: only root_path checking left the stub stranded
///   beside the real daemon group, and the empty root then broke the
///   next `n` keystroke against the real group).
#[derive(Debug, Clone)]
struct PendingProject {
    id: String,
    root_path: String,
    display_name: String,
    /// Daemon-assigned `projects.id` we observed for this `root_path`
    /// (set lazily after the first `sessions.create` succeeds). Used
    /// as a fallback eviction key when the daemon-side group's
    /// `root_path` is empty.
    daemon_project_id: Option<String>,
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
        let pending_bg = pending_projects.clone();
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
                    reconnect_loop(socket, cache_bg, pending_bg, refresh_gen_bg, cmd_rx).await;
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

    /// Test-only: directly seed the live cache. Used by the snapshot
    /// merge unit tests to mimic a daemon-side `sessions.list` push
    /// without spinning a real daemon. Not part of the production
    /// surface — production sources only mutate the cache through
    /// `refresh_cache`.
    #[doc(hidden)]
    pub fn __test_set_cache(&self, groups: Vec<ProjectGroup>) {
        if let Ok(mut c) = self.cache.lock() {
            *c = groups;
        }
    }

    /// Test-only: stamp a daemon-assigned project id onto the pending
    /// stub whose `root_path` matches `project_dir`. Mirrors what the
    /// bg loop does after a successful `sessions.create`; unit tests
    /// drive it directly because we don't spin a real daemon.
    #[doc(hidden)]
    pub fn __test_stamp_daemon_project_id(&self, project_dir: &str, daemon_project_id: &str) {
        if let Ok(mut pending) = self.pending_projects.lock() {
            let norm = normalize_root(project_dir);
            if let Some(stub) = pending
                .iter_mut()
                .find(|p| normalize_root(&p.root_path) == norm)
            {
                stub.daemon_project_id = Some(daemon_project_id.to_string());
            }
        }
    }
}

impl SessionSource for RpcSessionSource {
    fn snapshot(&self) -> Vec<ProjectGroup> {
        let mut groups = self.cache.lock().map(|c| c.clone()).unwrap_or_default();
        // Merge locally-registered "pending" projects (created via
        // `create_project` but not yet surfaced by the daemon's
        // `sessions.list`). Two evictions cooperate so the stub disappears
        // the moment the daemon-side group lands, even when the daemon
        // group's `root_path` is empty:
        //
        // 1. `root_path` match — works for worktree-first sessions, where
        //    `SessionSummary.worktree_path` populated the cache's
        //    `root_path`.
        // 2. `daemon_project_id` match — populated by the bg loop's
        //    create-handler when the freshly-spawned session's
        //    `project_id` surfaces in the refreshed cache. This covers
        //    the non-worktree-first case where `sessions.list` returns
        //    `worktree_path: None` and `build_groups` derives an empty
        //    `root_path` — the Code Reviewer's blocker on PR #92 round 2.
        //
        // We also OVERLAY the stub's `root_path` / `display_name` onto
        // the daemon group when the daemon side has an empty root. That
        // way the next `n` keystroke against the real group (whose
        // `project_id` is the daemon id, not the stub's id) still hits
        // a populated `root_path` and Confirm doesn't fall through to
        // the "project directory is missing" Validation arm.
        if let Ok(mut pending) = self.pending_projects.lock() {
            let mut overlays: Vec<(String, String, String)> = Vec::new();
            pending.retain(|p| {
                let matched = groups.iter().find(|g| {
                    let by_root = normalize_root(&g.root_path) == normalize_root(&p.root_path)
                        && !p.root_path.is_empty();
                    let by_pid = p
                        .daemon_project_id
                        .as_deref()
                        .map(|pid| g.project_id == pid)
                        .unwrap_or(false);
                    by_root || by_pid
                });
                if let Some(g) = matched {
                    if g.root_path.is_empty() && !p.root_path.is_empty() {
                        overlays.push((
                            g.project_id.clone(),
                            p.root_path.clone(),
                            p.display_name.clone(),
                        ));
                    }
                    false
                } else {
                    true
                }
            });
            for (pid, root, display) in overlays {
                if let Some(g) = groups.iter_mut().find(|g| g.project_id == pid) {
                    if g.root_path.is_empty() {
                        g.root_path = root;
                    }
                    if g.display_name.is_empty() {
                        g.display_name = display;
                    }
                }
            }
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
        // Duplicate guard: refuse only when the SAME root path is
        // already known (live cache OR pending list). Compare by
        // normalized `root_path` — never by `display_name`, since the
        // `projects` table only enforces uniqueness on `root_path`
        // (la-storage/migrations/0001_initial.sql) and two different
        // checkouts can legitimately share a basename
        // (`/repo/app` vs `/tmp/app`). The Code Reviewer's blocker on
        // PR #92 was the basename collision rejecting that second case.
        let norm = normalize_root(trimmed);
        let live_has = self
            .cache
            .lock()
            .map(|c| c.iter().any(|g| normalize_root(&g.root_path) == norm))
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
            if pending.iter().any(|p| normalize_root(&p.root_path) == norm) {
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
                daemon_project_id: None,
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
    pending: Arc<Mutex<Vec<PendingProject>>>,
    refresh_gen: Arc<AtomicU64>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let mut connected = false;
        match run_once(
            &socket,
            &cache,
            &pending,
            &refresh_gen,
            &mut cmd_rx,
            &mut connected,
        )
        .await
        {
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
    pending: &Arc<Mutex<Vec<PendingProject>>>,
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
                    let project_dir_for_pending = new_req.project_dir.clone();
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
                                // Link the daemon-assigned project_id
                                // back to whichever pending stub owns
                                // this directory. This is the eviction
                                // signal for the non-worktree-first
                                // case, where the daemon group's
                                // `root_path` would otherwise be empty
                                // (`sessions.list` only carries
                                // `worktree_path`, see
                                // `crates/la-daemon/src/dispatcher.rs`
                                // `handle_sessions_list`). Without this
                                // the stub would live forever next to
                                // the real group — Code Reviewer's
                                // blocker on PR #92 round 2.
                                stamp_pending_project_id(
                                    cache,
                                    pending,
                                    &project_dir_for_pending,
                                    &r.session_id,
                                );
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

/// Normalize a project root path for `pending_projects` dedup. Strips
/// trailing separators so `"/repo/app"` and `"/repo/app/"` compare
/// equal — the rest of the byte sequence is taken verbatim because the
/// TUI does not own the on-disk authority (the daemon's
/// `ensure_project` is the canonical normalizer, and it just stores
/// whatever string `sessions.create.project_dir` arrived with). Case is
/// preserved on every platform; case-folding `/Users/Alice` vs
/// `/users/alice` is the daemon's call, not ours.
fn normalize_root(p: &str) -> String {
    p.trim_end_matches(['/', '\\']).to_string()
}

/// After a successful `sessions.create`, look up the freshly-spawned
/// session's `project_id` in the just-refreshed cache and pin it onto
/// whichever pending stub owns the same `root_path`. This is what
/// evicts the stub in [`SessionSource::snapshot`] for the
/// non-worktree-first case (where the daemon group's `root_path` is
/// empty because `sessions.list` only ferries `worktree_path` and
/// `build_groups` derives an empty root from `None`).
///
/// Best-effort: if we can't find the session in the cache, or no
/// pending stub matches, we silently no-op — the snapshot's root_path
/// path still works for the worktree-first case, and the basic safety
/// net (`projects` table dedup at daemon level) means we never lose
/// data, just see two visible groups for one project until the user
/// restarts `la`. The Code Reviewer's blocker on PR #92 round 2 was
/// about that visible duplication, which this stamp prevents.
fn stamp_pending_project_id(
    cache: &Arc<Mutex<Vec<ProjectGroup>>>,
    pending: &Arc<Mutex<Vec<PendingProject>>>,
    project_dir: &str,
    session_id: &str,
) {
    let Ok(cache_guard) = cache.lock() else {
        return;
    };
    let Some(daemon_project_id) = cache_guard.iter().find_map(|g| {
        g.sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|_| g.project_id.clone())
    }) else {
        return;
    };
    drop(cache_guard);
    let Ok(mut pending_guard) = pending.lock() else {
        return;
    };
    let norm = normalize_root(project_dir);
    if let Some(stub) = pending_guard
        .iter_mut()
        .find(|p| normalize_root(&p.root_path) == norm)
    {
        stub.daemon_project_id = Some(daemon_project_id);
    }
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

    #[test]
    fn normalize_root_strips_trailing_separators_only() {
        // Code Reviewer's blocker on PR #92: dedup must match the
        // daemon's own uniqueness rule, which is "the literal
        // `root_path` string". Trailing-slash variants are the only
        // normalization the daemon's `ensure_project` is robust to
        // (PathBuf comparisons are identity), so we mirror that.
        assert_eq!(normalize_root("/repo/app"), "/repo/app");
        assert_eq!(normalize_root("/repo/app/"), "/repo/app");
        assert_eq!(normalize_root("/repo/app///"), "/repo/app");
        // Case is preserved on every platform — daemon decides if it
        // wants to case-fold, not the TUI.
        assert_eq!(normalize_root("/Users/Alice"), "/Users/Alice");
        assert_ne!(
            normalize_root("/Users/Alice"),
            normalize_root("/users/alice")
        );
        // Two distinct paths with the same basename must NOT compare
        // equal — that was the regression: `/repo/app` was collapsing
        // with `/tmp/app` because the old guard looked at display_name.
        assert_ne!(normalize_root("/repo/app"), normalize_root("/tmp/app"));
    }

    /// Construct an `RpcSessionSource` whose bg thread points at a
    /// path we never bind. The thread loops forever trying to connect;
    /// the test only exercises the synchronous trait surface (which
    /// does NOT need a live daemon for `snapshot`, `create_project`,
    /// or the `__test_set_cache` / `__test_stamp_daemon_project_id`
    /// helpers).
    fn fake_rpc_source() -> RpcSessionSource {
        let nowhere = std::path::Path::new("/tmp/lazyagents-no-such-socket-for-tests.sock");
        RpcSessionSource::connect(nowhere)
    }

    #[test]
    fn pending_stub_evicts_when_daemon_group_lands_with_matching_root_path() {
        // Worktree-first case: the daemon-side group carries a real
        // `root_path` (derived from `SessionSummary.worktree_path`).
        // Pure root-path match must evict the stub.
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().display().to_string();
        let mut src = fake_rpc_source();
        src.create_project(&path).expect("stub registered");
        let mut g = ProjectGroup::new("daemon-proj-id".to_string(), "proj-a".to_string());
        g.root_path = path.clone();
        src.__test_set_cache(vec![g]);
        let snap = src.snapshot();
        let roots: Vec<&str> = snap.iter().map(|g| g.root_path.as_str()).collect();
        let ids: Vec<&str> = snap.iter().map(|g| g.project_id.as_str()).collect();
        assert_eq!(
            roots.iter().filter(|r| **r == path.as_str()).count(),
            1,
            "expected exactly one group for {path} after merge; got roots={roots:?} ids={ids:?}",
        );
        assert!(
            ids.iter().any(|i| *i == "daemon-proj-id"),
            "real daemon group must survive; ids={ids:?}",
        );
        assert!(
            !ids.iter().any(|i| i.starts_with("pending-")),
            "pending stub must be evicted when root_path matches; ids={ids:?}",
        );
    }

    #[test]
    fn pending_stub_evicts_when_daemon_group_has_empty_root_path() {
        // Non-worktree-first case (Code Reviewer's blocker on PR #92
        // round 2): the daemon-side group's `root_path` is empty
        // because `sessions.list` returned `worktree_path: None`. The
        // stub MUST still evict via the daemon_project_id stamp the
        // bg loop applies after `sessions.create` succeeds.
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().display().to_string();
        let expected_display = td
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut src = fake_rpc_source();
        src.create_project(&path).expect("stub registered");
        // Stamp comes from the bg loop's create-handler in production.
        src.__test_stamp_daemon_project_id(&path, "daemon-proj-b-id");
        let mut g = ProjectGroup::new("daemon-proj-b-id".to_string(), String::new());
        // root_path stays empty — exactly what `build_groups` produces
        // for a SessionSummary { worktree_path: None }.
        g.root_path = String::new();
        src.__test_set_cache(vec![g]);
        let snap = src.snapshot();
        let ids: Vec<&str> = snap.iter().map(|g| g.project_id.as_str()).collect();
        assert!(
            !ids.iter().any(|i| i.starts_with("pending-")),
            "pending stub must NOT linger next to the daemon group; ids={ids:?}",
        );
        assert_eq!(ids, vec!["daemon-proj-b-id"]);
        // The merge also overlays the stub's root onto the empty
        // daemon group so a follow-up `n` against the real group
        // still has a usable `project_dir` (without this the App's
        // `on_new_session` reads "" and Confirm fails Validation).
        assert_eq!(
            snap[0].root_path, path,
            "empty daemon root should be overlaid with the stub's known path",
        );
        assert_eq!(
            snap[0].display_name, expected_display,
            "empty daemon display name should be overlaid with the stub's basename",
        );
    }

    #[test]
    fn snapshot_keeps_pending_when_no_daemon_match_yet() {
        // Sanity guard so the eviction doesn't over-fire and erase
        // stubs that haven't been registered with the daemon yet.
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().display().to_string();
        let mut src = fake_rpc_source();
        src.create_project(&path).expect("stub registered");
        // No matching cache entry yet.
        src.__test_set_cache(vec![]);
        let snap = src.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].project_id.starts_with("pending-"));
        assert_eq!(snap[0].root_path, path);
    }
}

// Re-exported for the integration test in `tests/rpc_source_live.rs`.
