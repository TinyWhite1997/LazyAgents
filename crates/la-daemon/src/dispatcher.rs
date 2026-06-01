//! Per-connection JSON-RPC dispatcher.
//!
//! Each accepted client connection runs one [`serve_connection`] task. The
//! task:
//!
//! 1. Performs the [`la_ipc::server_handshake`] (architecture §3 握手).
//! 2. Splits the connection into read/write halves.
//! 3. Spins a writer task that drains:
//!    - any subscriptions opened by `sessions.attach` (push
//!      `session.output` / `session.gap`), AND
//!    - the global [`la_core::EventBus`] for the topics the client picked
//!      via `events.subscribe` (push `session.state` / `daemon.health`).
//! 4. Loops over incoming `Request`s, dispatches by `method`, and writes
//!    the typed response.
//!
//! Connection close cleans up subscriptions and writer ownership the
//! client held; the session itself keeps running (architecture §1.2
//! invariant "la 永远不直接持有 PTY"). The shutdown token from
//! [`crate::signals`] aborts the read loop so the daemon can stop quickly
//! when SIGTERM lands.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use la_adapter::{AgentAdapter, SpawnRequest, StdinMode};
use la_core::{BusEvent, CoreError, SessionId, SessionManager};
use la_ipc::{server_handshake, Connection, HubEvent, SendHalf, SubId, Subscription};
use la_proto::error_codes;
use la_proto::jsonrpc::{Message, Notification, Request, Response, RpcError};
use la_proto::methods::{
    EventTopic, EventsSubscribeParams, EventsSubscribeResult, Method, PtySize as ProtoPtySize,
    ServerCapabilities, SessionState, SessionSummary, SessionsAttachParams, SessionsAttachResult,
    SessionsCreateParams, SessionsCreateResult, SessionsDetachParams, SessionsDetachResult,
    SessionsListParams, SessionsListResult, SessionsSignalParams, SessionsSignalResult,
    SessionsWriteParams, SessionsWriteResult,
};
use la_proto::notifications::{
    DaemonHealth, NotificationMethod, SessionGap, SessionOutput, SessionStateNotice,
};
use la_proto::PROTOCOL_VERSION;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinSet;

/// Shared per-process state handed to every connection. Wraps the
/// [`SessionManager`], the adapter registry, and a clone-cheap notifier
/// the runtime fires to ask every connection to wind down.
#[derive(Clone)]
pub struct ConnectionContext {
    pub manager: SessionManager,
    pub adapters: AdapterRegistry,
    pub server_version: String,
    /// When `notified()`, every active connection should drop into a
    /// graceful close: stop reading new requests, finish what's in flight,
    /// flush the writer, and return.
    pub shutdown: Arc<Notify>,
}

/// A name → adapter lookup. Cheap to clone because every adapter sits
/// behind an `Arc`. The runtime registers `claude` (and any future
/// adapter) once at startup.
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    inner: Arc<HashMap<String, Arc<dyn AgentAdapter>>>,
}

impl AdapterRegistry {
    pub fn from_map(map: HashMap<String, Arc<dyn AgentAdapter>>) -> Self {
        Self {
            inner: Arc::new(map),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.inner.get(name).cloned()
    }

    /// Names of registered adapters, in arbitrary order. Surfaced in the
    /// initialize handshake (`ServerCapabilities.adapters`).
    pub fn names(&self) -> Vec<String> {
        self.inner.keys().cloned().collect()
    }
}

/// Type-erased sink so the per-connection state doesn't have to
/// propagate the stream type parameter into every helper signature.
#[async_trait]
trait MessageSink: Send + Sync {
    async fn send(&self, msg: &Message) -> Result<(), la_ipc::IpcError>;
}

#[async_trait]
impl<S> MessageSink for SendHalf<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    async fn send(&self, msg: &Message) -> Result<(), la_ipc::IpcError> {
        SendHalf::send(self, msg).await
    }
}

/// Serve a single accepted connection until the client closes it or the
/// shutdown notify fires. Returns once the connection is fully torn down
/// and any subscriptions / writer ownership are released.
pub async fn serve_connection<S>(stream: S, ctx: ConnectionContext)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    let mut conn = Connection::new(stream);

    let caps = ServerCapabilities {
        adapters: ctx.adapters.names(),
        cron: false,
        worktree: false,
        events: true,
    };
    let handshake = match server_handshake(
        &mut conn,
        crate::SERVER_NAME,
        &ctx.server_version,
        &[PROTOCOL_VERSION],
        caps,
    )
    .await
    {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(%err, "handshake failed");
            return;
        }
    };
    tracing::info!(
        client = %handshake.client,
        version = %handshake.client_version,
        "client connected"
    );

    let (send_half, mut recv_half) = conn.split();
    let send: Arc<dyn MessageSink> = Arc::new(send_half);
    let state = ConnState::new(ctx.manager.clone(), send);

    let writer_state = state.clone();
    let writer_ctx = ctx.clone();
    let writer_handle = tokio::spawn(async move {
        run_writer(writer_state, writer_ctx).await;
    });

    loop {
        let recv = recv_half.recv();
        let next = tokio::select! {
            r = recv => r,
            _ = ctx.shutdown.notified() => break,
        };
        match next {
            Ok(Some(Message::Request(req))) => {
                let response = handle_request(req, &state, &ctx).await;
                if let Err(err) = state.send.send(&Message::Response(response)).await {
                    tracing::debug!(%err, "response send failed; closing");
                    break;
                }
            }
            Ok(Some(Message::Notification(n))) => {
                tracing::debug!(method = %n.method, "ignoring client notification");
            }
            Ok(Some(Message::Response(_))) => {
                tracing::debug!("ignoring spurious response from client");
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(%err, "recv failed; closing");
                break;
            }
        }
    }

    state.shutdown.notify_waiters();
    let _ = writer_handle.await;
    state.release_all(&ctx.manager).await;
}

/// Per-connection mutable state shared between the reader and the writer
/// fan-out task.
#[derive(Clone)]
struct ConnState {
    inner: Arc<ConnStateInner>,
}

struct ConnStateInner {
    manager: SessionManager,
    send: Arc<dyn MessageSink>,
    attachments: RwLock<HashMap<SessionId, AttachmentSlot>>,
    /// Topics the client subscribed to via `events.subscribe`. The writer
    /// task reads this on every bus tick.
    topics: RwLock<TopicSet>,
    shutdown: Arc<Notify>,
    /// Notified whenever a new attachment is added so the writer task can
    /// pick it up without polling.
    attachments_changed: Arc<Notify>,
}

#[derive(Default, Clone)]
struct TopicSet {
    session_state: bool,
    daemon_health: bool,
}

struct AttachmentSlot {
    sub: Option<Subscription>,
    sub_id: SubId,
}

impl ConnState {
    fn new(manager: SessionManager, send: Arc<dyn MessageSink>) -> Self {
        Self {
            inner: Arc::new(ConnStateInner {
                manager,
                send,
                attachments: RwLock::new(HashMap::new()),
                topics: RwLock::new(TopicSet::default()),
                shutdown: Arc::new(Notify::new()),
                attachments_changed: Arc::new(Notify::new()),
            }),
        }
    }
}

impl std::ops::Deref for ConnState {
    type Target = ConnStateInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ConnState {
    async fn release_all(&self, manager: &SessionManager) {
        let mut attachments = self.attachments.write().await;
        for (id, slot) in attachments.drain() {
            if let Err(err) = manager.detach(&id, slot.sub_id).await {
                tracing::debug!(%err, session = %id.as_str(), "detach on close failed");
            }
        }
    }
}

async fn run_writer(state: ConnState, ctx: ConnectionContext) {
    let mut bus_rx = ctx.manager.bus().subscribe();
    let attach_changed = state.attachments_changed.clone();
    let shutdown = state.shutdown.clone();
    let mut active: HashMap<SessionId, ()> = HashMap::new();
    let mut sub_tasks: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => { break; },
            _ = attach_changed.notified() => {
                let new = collect_new_subs(&state, &mut active).await;
                for (id, sub) in new {
                    let send = state.send.clone();
                    sub_tasks.spawn(async move {
                        drain_subscription(id, sub, send).await;
                    });
                }
            },
            ev = bus_rx.recv() => {
                match ev {
                    Ok(event) => deliver_bus_event(&state, event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => { break; },
                }
            },
            Some(_joined) = sub_tasks.join_next() => {
                // Drain finished writer tasks so the JoinSet doesn't grow.
            }
        }
    }

    sub_tasks.abort_all();
}

async fn collect_new_subs(
    state: &ConnState,
    active: &mut HashMap<SessionId, ()>,
) -> Vec<(SessionId, Subscription)> {
    let mut new = Vec::new();
    let mut attachments = state.attachments.write().await;
    for (id, slot) in attachments.iter_mut() {
        if active.contains_key(id) {
            continue;
        }
        if let Some(sub) = slot.sub.take() {
            active.insert(id.clone(), ());
            new.push((id.clone(), sub));
        }
    }
    new
}

async fn drain_subscription(_id: SessionId, mut sub: Subscription, send: Arc<dyn MessageSink>) {
    while let Some(event) = sub.recv().await {
        let result = match event {
            HubEvent::Output(p) => Notification::new(SessionOutput::NAME, &*p),
            HubEvent::Gap(p) => Notification::new(SessionGap::NAME, &p),
        };
        match result {
            Ok(n) => {
                if let Err(err) = send.send(&Message::Notification(n)).await {
                    tracing::debug!(%err, "sub notification send failed");
                    break;
                }
            }
            Err(err) => {
                tracing::warn!(%err, "encode notification failed");
                break;
            }
        }
    }
}

async fn deliver_bus_event(state: &ConnState, event: BusEvent) {
    let topics = state.topics.read().await.clone();
    let notification = match event {
        BusEvent::SessionState(p) if topics.session_state => {
            Notification::new(SessionStateNotice::NAME, &p)
        }
        BusEvent::DaemonHealth(p) if topics.daemon_health => {
            Notification::new(DaemonHealth::NAME, &p)
        }
        _ => return,
    };
    match notification {
        Ok(n) => {
            if let Err(err) = state.send.send(&Message::Notification(n)).await {
                tracing::debug!(%err, "bus notification send failed");
            }
        }
        Err(err) => tracing::warn!(%err, "encode bus notification failed"),
    }
}

async fn handle_request(req: Request, state: &ConnState, ctx: &ConnectionContext) -> Response {
    let id = req.id.clone();
    let method = req.method.clone();
    let result = dispatch(req, state, ctx).await;
    match result {
        Ok(value) => Response {
            jsonrpc: la_proto::jsonrpc::Version,
            id,
            outcome: la_proto::jsonrpc::ResponseOutcome::Result(value),
        },
        Err(err) => {
            tracing::debug!(method = %method, code = err.code, msg = %err.message, "rpc error");
            Response::error(id, err)
        }
    }
}

async fn dispatch(
    req: Request,
    state: &ConnState,
    ctx: &ConnectionContext,
) -> Result<serde_json::Value, RpcError> {
    use la_proto::methods::{
        Initialize, SessionsArchive, SessionsAttach, SessionsCreate, SessionsDelete,
        SessionsDetach, SessionsList, SessionsSignal, SessionsWrite,
    };

    match req.method.as_str() {
        Initialize::NAME => Err(RpcError::invalid_request(
            "initialize already handled during handshake",
        )),
        "events.subscribe" => {
            let params: EventsSubscribeParams = decode_params(req)?;
            handle_events_subscribe(state, params).await
        }
        SessionsList::NAME => {
            let params: SessionsListParams = decode_params(req)?;
            handle_sessions_list(state, params).await
        }
        SessionsCreate::NAME => {
            let params: SessionsCreateParams = decode_params(req)?;
            handle_sessions_create(state, ctx, params).await
        }
        SessionsAttach::NAME => {
            let params: SessionsAttachParams = decode_params(req)?;
            handle_sessions_attach(state, params).await
        }
        SessionsDetach::NAME => {
            let params: SessionsDetachParams = decode_params(req)?;
            handle_sessions_detach(state, params).await
        }
        SessionsWrite::NAME => {
            let params: SessionsWriteParams = decode_params(req)?;
            handle_sessions_write(state, params).await
        }
        SessionsSignal::NAME => {
            let params: SessionsSignalParams = decode_params(req)?;
            handle_sessions_signal(state, params).await
        }
        SessionsArchive::NAME => {
            let params = decode_params::<la_proto::methods::SessionsArchiveParams>(req)?;
            let id = SessionId(params.session_id);
            state.manager.archive(&id).await.map_err(core_to_rpc)?;
            ok(la_proto::methods::SessionsArchiveResult {})
        }
        SessionsDelete::NAME => {
            let params = decode_params::<la_proto::methods::SessionsDeleteParams>(req)?;
            let id = SessionId(params.session_id);
            state.manager.delete(&id).await.map_err(core_to_rpc)?;
            ok(la_proto::methods::SessionsDeleteResult {})
        }
        "shutdown" => ok(la_proto::methods::ShutdownResult {}),
        other => Err(RpcError::method_not_found(other)),
    }
}

fn decode_params<T: serde::de::DeserializeOwned>(req: Request) -> Result<T, RpcError> {
    req.params_into().map_err(|e| {
        RpcError::invalid_params(format!("decode {}: {e}", std::any::type_name::<T>()))
    })
}

fn ok<R: serde::Serialize>(r: R) -> Result<serde_json::Value, RpcError> {
    serde_json::to_value(r).map_err(|e| RpcError::internal_error(format!("encode result: {e}")))
}

async fn handle_events_subscribe(
    state: &ConnState,
    params: EventsSubscribeParams,
) -> Result<serde_json::Value, RpcError> {
    let mut topics = state.topics.write().await;
    let mut effective = Vec::new();
    for t in &params.topics {
        match t {
            EventTopic::SessionState => {
                topics.session_state = true;
                effective.push(*t);
            }
            EventTopic::DaemonHealth => {
                topics.daemon_health = true;
                effective.push(*t);
            }
            EventTopic::SessionOutput | EventTopic::SessionGap => {
                // Per-session topics are delivered through sessions.attach,
                // not the global bus; ack but don't echo them back so
                // clients don't think they have a global subscription.
            }
            EventTopic::CronFired => {
                // Cron isn't implemented until M3; quietly omit from the
                // effective set (architecture §3 documented behaviour).
            }
        }
    }
    ok(EventsSubscribeResult { topics: effective })
}

async fn handle_sessions_list(
    state: &ConnState,
    params: SessionsListParams,
) -> Result<serde_json::Value, RpcError> {
    let storage = state.manager.storage();
    let rows = if let Some(project) = params.project.as_deref() {
        storage
            .sessions()
            .list_by_project(project, params.include_archived)
            .await
            .map_err(storage_to_rpc)?
    } else {
        let projects = storage.projects().list().await.map_err(storage_to_rpc)?;
        let mut all = Vec::new();
        for p in projects {
            let rows = storage
                .sessions()
                .list_by_project(&p.id, params.include_archived)
                .await
                .map_err(storage_to_rpc)?;
            all.extend(rows);
        }
        all
    };

    let mut sessions: Vec<SessionSummary> = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(backend) = params.backend.as_deref() {
            if row.backend_id != backend {
                continue;
            }
        }
        sessions.push(SessionSummary {
            session_id: row.id,
            project_id: row.project_id,
            backend: row.backend_id,
            title: row.title,
            state: parse_state(&row.state),
            origin: row.origin,
            created_at: row.created_at,
            updated_at: row.updated_at,
            worktree_path: row.worktree_path,
        });
    }
    ok(SessionsListResult { sessions })
}

async fn handle_sessions_create(
    state: &ConnState,
    ctx: &ConnectionContext,
    params: SessionsCreateParams,
) -> Result<serde_json::Value, RpcError> {
    let adapter = ctx.adapters.get(&params.backend).ok_or_else(|| {
        RpcError::new(
            error_codes::ADAPTER_NOT_INSTALLED,
            format!("no adapter registered for backend {:?}", params.backend),
        )
    })?;

    let project_id = ensure_project(state, &params.project_dir).await?;

    let mut req = SpawnRequest::new(params.project_dir.clone());
    req.extra_args = params.args.iter().map(std::ffi::OsString::from).collect();
    req.prompt = params.prompt;
    req.stdin_mode = StdinMode::Pty;

    let spawned = state
        .manager
        .spawn(&*adapter, project_id, req)
        .await
        .map_err(core_to_rpc)?;

    let initial_pty = ProtoPtySize {
        rows: 32,
        cols: 120,
    };
    ok(SessionsCreateResult {
        session_id: spawned.id.0.clone(),
        backend: spawned.backend,
        cwd: params.project_dir,
        initial_size: initial_pty,
        state: spawned.initial_state,
    })
}

async fn ensure_project(state: &ConnState, dir: &str) -> Result<String, RpcError> {
    let storage = state.manager.storage();
    if let Some(existing) = storage
        .projects()
        .get_by_root_path(dir)
        .await
        .map_err(storage_to_rpc)?
    {
        return Ok(existing.id);
    }
    let id = la_storage::new_id();
    let display = std::path::Path::new(dir)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string());
    storage
        .projects()
        .create(la_storage::NewProject {
            id: id.clone(),
            root_path: dir.to_string(),
            display_name: display,
            vcs: None,
        })
        .await
        .map_err(storage_to_rpc)?;
    Ok(id)
}

async fn handle_sessions_attach(
    state: &ConnState,
    params: SessionsAttachParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id.clone());
    let outcome = state
        .manager
        .attach(&id, None, params.acquire_input)
        .await
        .map_err(core_to_rpc)?;

    let sub_id = outcome.subscription.id();
    {
        let mut attachments = state.attachments.write().await;
        attachments.insert(
            id,
            AttachmentSlot {
                sub: Some(outcome.subscription),
                sub_id,
            },
        );
    }
    state.attachments_changed.notify_one();

    ok(SessionsAttachResult {
        session_id: params.session_id,
        snapshot_seq: outcome.snapshot_seq,
        input_acquired: outcome.input_acquired,
    })
}

async fn handle_sessions_detach(
    state: &ConnState,
    params: SessionsDetachParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id);
    let slot = {
        let mut attachments = state.attachments.write().await;
        attachments.remove(&id)
    };
    if let Some(slot) = slot {
        state
            .manager
            .detach(&id, slot.sub_id)
            .await
            .map_err(core_to_rpc)?;
    }
    ok(SessionsDetachResult {})
}

async fn handle_sessions_write(
    state: &ConnState,
    params: SessionsWriteParams,
) -> Result<serde_json::Value, RpcError> {
    let bytes = params
        .data_bytes()
        .map_err(|e| RpcError::invalid_params(format!("data_base64 decode: {e}")))?;
    let id = SessionId(params.session_id);
    let sub_id = {
        let attachments = state.attachments.read().await;
        attachments
            .get(&id)
            .map(|slot| slot.sub_id)
            .ok_or_else(|| {
                RpcError::new(
                    error_codes::NOT_ATTACHED,
                    "sessions.attach required before sessions.write",
                )
            })?
    };
    state
        .manager
        .write(&id, sub_id, &bytes)
        .await
        .map_err(core_to_rpc)?;
    ok(SessionsWriteResult {})
}

async fn handle_sessions_signal(
    state: &ConnState,
    params: SessionsSignalParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id);
    state
        .manager
        .signal(&id, params.signal)
        .await
        .map_err(core_to_rpc)?;
    ok(SessionsSignalResult {})
}

fn parse_state(s: &str) -> SessionState {
    match s {
        "starting" => SessionState::Starting,
        "running" => SessionState::Running,
        "waiting" => SessionState::Waiting,
        "errored" => SessionState::Errored,
        "archived" => SessionState::Archived,
        _ => SessionState::Exited,
    }
}

fn core_to_rpc(err: CoreError) -> RpcError {
    // CoreError already knows its wire kind; preserve the message verbatim
    // so the JSON-RPC body matches what la-core wrote into the variant.
    let message = err.to_string();
    RpcError::new(err.kind().code(), message)
}

fn storage_to_rpc(err: la_storage::StorageError) -> RpcError {
    use la_storage::StorageError as E;
    match err {
        E::Busy { .. } => RpcError::new(error_codes::STORAGE_BUSY, err.to_string()),
        E::MissingSession(id) => RpcError::new(
            error_codes::SESSION_NOT_FOUND,
            format!("missing session: {id}"),
        ),
        E::MissingProject(id) => RpcError::new(
            error_codes::STORAGE_CONFLICT,
            format!("missing project: {id}"),
        ),
        _ => RpcError::new(error_codes::STORAGE_FAILED, err.to_string()),
    }
}
