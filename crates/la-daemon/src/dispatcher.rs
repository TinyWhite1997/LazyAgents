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
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use la_adapter::{
    AdapterError, AgentAdapter, DiscoverHints, DiscoveredSession as AdapterDiscoveredSession,
    SpawnRequest, StdinMode,
};
use la_core::{BusEvent, CoreError, SessionId, SessionManager};
use la_ipc::{server_handshake, Connection, HubEvent, SendHalf, SubId, Subscription};
use la_proto::error_codes;
use la_proto::jsonrpc::{Message, Notification, Request, Response, RpcError};
use la_proto::methods::{
    AdaptersDiscoverParams, AdaptersDiscoverResult, DiscoveredSession as ProtoDiscoveredSession,
    EventTopic, EventsSubscribeParams, EventsSubscribeResult, ImportedSession, Method,
    PtySize as ProtoPtySize, ServerCapabilities, SessionState, SessionSummary,
    SessionsAttachParams, SessionsAttachResult, SessionsCreateParams, SessionsCreateResult,
    SessionsDetachParams, SessionsDetachResult, SessionsImportParams, SessionsImportResult,
    SessionsListParams, SessionsListResult, SessionsSignalParams, SessionsSignalResult,
    SessionsWriteParams, SessionsWriteResult, WorktreeCommit, WorktreeCommitParams,
    WorktreeCommitResult, WorktreeDiff, WorktreeDiffParams, WorktreeDiffResult, WorktreeDiscard,
    WorktreeMutationParams, WorktreeMutationResult, WorktreeOpenInEditor,
    WorktreeOpenInEditorParams, WorktreeOpenInEditorResult, WorktreeStage, WorktreeStatus,
    WorktreeStatusParams, WorktreeStatusResult, WorktreeUnstage,
};
use la_proto::notifications::{
    CronFired, DaemonHealth, NotificationMethod, SchedulerHealth, SessionGap, SessionOutput,
    SessionStateNotice, WorktreeChanged, WorktreeChangedParams, WorktreeCommitCreated,
    WorktreeCommitCreatedParams,
};
use la_proto::PROTOCOL_VERSION;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinSet;
use tracing::Instrument;

/// Shared per-process state handed to every connection. Wraps the
/// [`SessionManager`], the adapter registry, and a clone-cheap notifier
/// the runtime fires to ask every connection to wind down.
#[derive(Clone)]
pub struct ConnectionContext {
    pub manager: SessionManager,
    pub adapters: AdapterRegistry,
    /// Cached probe results per backend, refreshed by the daemon's
    /// background `daemon.health` loop (`WEK-29`). Connection handlers
    /// read this to refuse `sessions.create` against an unavailable
    /// backend with the right `-33101` / `-33102` code before the call
    /// even reaches `la-core`.
    pub health: crate::health::HealthRegistry,
    /// WEK-57 / M3.9: handle into the scheduler stack so `crons.* /
    /// runs.*` mutations can route through the single scheduler control
    /// channel + admission lock.
    pub scheduler: Arc<crate::scheduler::SchedulerServices>,
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

    /// `(id, adapter)` pairs — used by the health probe loop in
    /// `WEK-29` so it can run `adapter.probe()` against every
    /// registered backend without re-wrapping the inner `HashMap`.
    pub fn pairs(&self) -> Vec<(String, Arc<dyn AgentAdapter>)> {
        self.inner
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
        // WEK-57 / M3.9: scheduler is wired up. `crons.* / runs.*` route
        // through the daemon's `SchedulerServices` and the executor's
        // single admission gate.
        cron: true,
        // WEK-27: signal that `sessions.create { worktree: true }` is
        // wired and honoured. `Daemon::bind` always constructs a
        // `WorktreeManager`, so this is unconditionally `true` from M2
        // onwards; if a future build wants to disable worktrees it
        // should clear `ManagerConfig.worktree` AND drop this bit.
        worktree: true,
        // WEK-28: `worktree.status` / `worktree.diff` / `worktree.stage`
        // / `worktree.unstage` / `worktree.discard` / `worktree.commit`
        // / `worktree.open_in_editor` are all routed below.
        diff: true,
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
    scheduler_health: bool,
    worktree_changed: bool,
    worktree_commit: bool,
    cron_fired: bool,
}

struct AttachmentSlot {
    sub: Option<Subscription>,
    sub_id: SubId,
    /// Backend id (`claude`, `codex`, ...) attached to this session. Used
    /// to label `lad_session_output_bytes_total{backend}` so the gauge
    /// matches the A9 metric naming table. Looked up from storage at
    /// `sessions.attach` time and cached for the life of the attachment.
    backend: String,
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
                for (id, sub, backend) in new {
                    let send = state.send.clone();
                    sub_tasks.spawn(async move {
                        drain_subscription(id, backend, sub, send).await;
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
) -> Vec<(SessionId, Subscription, String)> {
    let mut new = Vec::new();
    let mut attachments = state.attachments.write().await;
    for (id, slot) in attachments.iter_mut() {
        if active.contains_key(id) {
            continue;
        }
        if let Some(sub) = slot.sub.take() {
            active.insert(id.clone(), ());
            new.push((id.clone(), sub, slot.backend.clone()));
        }
    }
    new
}

async fn drain_subscription(
    _id: SessionId,
    backend: String,
    mut sub: Subscription,
    send: Arc<dyn MessageSink>,
) {
    while let Some(event) = sub.recv().await {
        let result = match event {
            HubEvent::Output(p) => {
                if let Ok(bytes) = p.data_bytes() {
                    metrics::counter!(
                        "lad_session_output_bytes_total",
                        "backend" => backend.clone(),
                    )
                    .increment(bytes.len() as u64);
                }
                Notification::new(SessionOutput::NAME, &*p)
            }
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
        BusEvent::SchedulerHealth(p) if topics.scheduler_health => {
            Notification::new(SchedulerHealth::NAME, &p)
        }
        BusEvent::WorktreeChanged(p) if topics.worktree_changed => {
            Notification::new(WorktreeChanged::NAME, &p)
        }
        BusEvent::WorktreeCommitCreated(p) if topics.worktree_commit => {
            Notification::new(WorktreeCommitCreated::NAME, &p)
        }
        BusEvent::CronFired(p) if topics.cron_fired => {
            tracing::info!(
                trace_id = %la_observ::new_trace_id(),
                cron_id = %p.cron_id,
                run_id = %p.run_id,
                status = %p.status,
                "cron fired notification delivered",
            );
            Notification::new(CronFired::NAME, &p)
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
    let trace_id = la_observ::new_trace_id();
    let span = tracing::info_span!("rpc", trace_id = %trace_id, method = %method);
    let started = std::time::Instant::now();
    let result = dispatch(req, state, ctx).instrument(span).await;
    let elapsed = started.elapsed().as_secs_f64();
    let result_label = if result.is_ok() { "ok" } else { "error" };
    // M4.5 / WEK-75 — A9 metric naming table: every RPC call emits
    // `lad_rpc_requests_total{method,result}` + a `lad_rpc_duration_seconds`
    // histogram observation, labelled by method (no `result` label on the
    // histogram — keeping cardinality bounded matters more than slicing
    // success/error latency, which the dispatcher logs already carry).
    metrics::counter!(
        "lad_rpc_requests_total",
        "method" => method.clone(),
        "result" => result_label,
    )
    .increment(1);
    metrics::histogram!("lad_rpc_duration_seconds", "method" => method.clone()).record(elapsed);
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
        AdaptersDiscover, Initialize, ProjectsCreate, ProjectsList, SessionsArchive,
        SessionsAttach, SessionsCreate, SessionsDelete, SessionsDetach, SessionsImport,
        SessionsList, SessionsResize, SessionsSignal, SessionsWrite,
    };

    match req.method.as_str() {
        Initialize::NAME => Err(RpcError::invalid_request(
            "initialize already handled during handshake",
        )),
        "events.subscribe" => {
            let params: EventsSubscribeParams = decode_params(req)?;
            handle_events_subscribe(state, ctx, params).await
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
        SessionsResize::NAME => {
            let params: la_proto::methods::SessionsResizeParams = decode_params(req)?;
            handle_sessions_resize(state, params).await
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
        ProjectsList::NAME => {
            let _params = decode_params::<la_proto::methods::ProjectsListParams>(req)?;
            handle_projects_list(state).await
        }
        ProjectsCreate::NAME => {
            let params = decode_params::<la_proto::methods::ProjectsCreateParams>(req)?;
            handle_projects_create(state, params).await
        }
        AdaptersDiscover::NAME => {
            let params: AdaptersDiscoverParams = decode_params(req)?;
            handle_adapters_discover(state, ctx, params).await
        }
        SessionsImport::NAME => {
            let params: SessionsImportParams = decode_params(req)?;
            handle_sessions_import(state, ctx, params).await
        }
        WorktreeStatus::NAME => {
            let params: WorktreeStatusParams = decode_params(req)?;
            handle_worktree_status(state, params).await
        }
        WorktreeDiff::NAME => {
            let params: WorktreeDiffParams = decode_params(req)?;
            handle_worktree_diff(state, params).await
        }
        WorktreeStage::NAME => {
            let params: WorktreeMutationParams = decode_params(req)?;
            handle_worktree_mutation(state, params, WorktreeMutationKind::Stage).await
        }
        WorktreeUnstage::NAME => {
            let params: WorktreeMutationParams = decode_params(req)?;
            handle_worktree_mutation(state, params, WorktreeMutationKind::Unstage).await
        }
        WorktreeDiscard::NAME => {
            let params: WorktreeMutationParams = decode_params(req)?;
            handle_worktree_mutation(state, params, WorktreeMutationKind::Discard).await
        }
        WorktreeCommit::NAME => {
            let params: WorktreeCommitParams = decode_params(req)?;
            handle_worktree_commit(state, params).await
        }
        WorktreeOpenInEditor::NAME => {
            let params: WorktreeOpenInEditorParams = decode_params(req)?;
            handle_worktree_open_in_editor(state, params).await
        }
        la_proto::methods::CronsList::NAME => {
            let params: la_proto::methods::CronsListParams = decode_params(req)?;
            handle_crons_list(state, params).await
        }
        la_proto::methods::CronsGet::NAME => {
            let params: la_proto::methods::CronsGetParams = decode_params(req)?;
            handle_crons_get(state, params).await
        }
        la_proto::methods::CronsUpsert::NAME => {
            let params: la_proto::methods::CronsUpsertParams = decode_params(req)?;
            handle_crons_upsert(state, ctx, params).await
        }
        la_proto::methods::CronsDelete::NAME => {
            let params: la_proto::methods::CronsDeleteParams = decode_params(req)?;
            handle_crons_delete(state, ctx, params).await
        }
        la_proto::methods::CronsSetEnabled::NAME => {
            let params: la_proto::methods::CronsSetEnabledParams = decode_params(req)?;
            handle_crons_set_enabled(state, ctx, params).await
        }
        la_proto::methods::CronsRunNow::NAME => {
            let params: la_proto::methods::CronsRunNowParams = decode_params(req)?;
            handle_crons_run_now(state, ctx, params).await
        }
        la_proto::methods::CronsDryRun::NAME => {
            let params: la_proto::methods::CronsDryRunParams = decode_params(req)?;
            handle_crons_dry_run(params)
        }
        la_proto::methods::RunsList::NAME => {
            let params: la_proto::methods::RunsListParams = decode_params(req)?;
            handle_runs_list(state, params).await
        }
        la_proto::methods::RunsGet::NAME => {
            let params: la_proto::methods::RunsGetParams = decode_params(req)?;
            handle_runs_get(state, params).await
        }
        la_proto::methods::MetricsScrape::NAME => {
            // M4.5 / WEK-75 — A9 metrics.scrape 三层一致性 (daemon RPC handler
            // 层): render the in-process `metrics-exporter`-style recorder
            // straight from `la-observ`. The body is whatever
            // `render_prometheus` produces, unmodified — the CLI then
            // prints it byte-identical so `lad metrics` and a future
            // direct RPC client agree on the exposition text.
            let _params: la_proto::methods::MetricsScrapeParams = decode_params(req)?;
            ok(la_proto::methods::MetricsScrapeResult {
                body: la_observ::render_prometheus(),
            })
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
    ctx: &ConnectionContext,
    params: EventsSubscribeParams,
) -> Result<serde_json::Value, RpcError> {
    let mut effective = Vec::new();
    let mut send_health_snapshot = false;
    {
        let mut topics = state.topics.write().await;
        for t in &params.topics {
            match t {
                EventTopic::SessionState => {
                    topics.session_state = true;
                    effective.push(*t);
                }
                EventTopic::DaemonHealth => {
                    // Only replay the snapshot on a fresh subscribe. A
                    // re-subscribe (already flagged true) is a no-op so we
                    // don't double-push.
                    if !topics.daemon_health {
                        send_health_snapshot = true;
                    }
                    topics.daemon_health = true;
                    effective.push(*t);
                }
                EventTopic::SchedulerHealth => {
                    // No initial-snapshot replay here: the scheduler
                    // health loop fires every 5 s by default, so the TUI
                    // gets a fresh frame quickly. If we ever extend the
                    // cadence past the user's tolerance (the original
                    // ProbeLoopConfig hit this issue at 60 s), bring back
                    // a snapshot path mirroring `send_health_snapshot`.
                    topics.scheduler_health = true;
                    effective.push(*t);
                }
                EventTopic::WorktreeChanged => {
                    topics.worktree_changed = true;
                    effective.push(*t);
                }
                EventTopic::WorktreeCommit => {
                    topics.worktree_commit = true;
                    effective.push(*t);
                }
                EventTopic::SessionOutput | EventTopic::SessionGap => {
                    // Per-session topics are delivered through sessions.attach,
                    // not the global bus; ack but don't echo them back so
                    // clients don't think they have a global subscription.
                }
                EventTopic::CronFired => {
                    // Scheduler integration lands later in M3, but the
                    // wire path is plumbed today so the TUI status bar
                    // (`↻ cron-id` pulse — WEK-36) shows pulses the
                    // moment the scheduler starts publishing to
                    // `BusEvent::CronFired`. Echo the topic back so the
                    // client knows the subscribe took.
                    topics.cron_fired = true;
                    effective.push(*t);
                }
            }
        }
    }

    // WEK-29 review fix: the broadcast bus does NOT replay messages sent
    // before this connection subscribed, so a TUI that connects after the
    // daemon's initial probe would otherwise wait up to
    // `DEFAULT_PROBE_INTERVAL` (60 s) for the next pulse before grey-
    // stating an uninstalled backend. Push the cached snapshot directly
    // on this connection so the very next `daemon.health` notification
    // the client sees carries the current backends.
    if send_health_snapshot {
        let backends = ctx.health.snapshot().await;
        let params = la_proto::notifications::DaemonHealthParams {
            queue_depth: 0,
            running: ctx.manager.active_count().await as u32,
            errors_last_5m: backends
                .iter()
                .filter(|b| b.status != la_proto::notifications::BackendHealthStatus::Available)
                .count() as u32,
            backends,
            managed_by: crate::install::detect_running_service(),
        };
        match Notification::new(DaemonHealth::NAME, &params) {
            Ok(n) => {
                if let Err(err) = state.send.send(&Message::Notification(n)).await {
                    tracing::debug!(%err, "initial daemon.health send failed");
                }
            }
            Err(err) => tracing::warn!(%err, "encode initial daemon.health failed"),
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

    // WEK-29 pre-flight: if the latest cached probe says the backend
    // isn't installed, fail fast with the right business code instead of
    // letting the spawn explode with a generic `AdapterSpawnFailed`. The
    // TUI uses this code to hide the backend rather than offering a
    // `create` entry point. Unauthenticated backends are intentionally
    // *not* blocked here — they can run against an API key, so we let the
    // spawn proceed and surface any real auth failure from the child.
    if let Some(probe) = ctx.health.probe_for(&params.backend).await {
        if crate::health::is_blocking(&probe) {
            return Err(probe_to_rpc(&params.backend, &probe));
        }
    }

    let project_id = ensure_project(state, &params.project_dir).await?;

    let mut req = SpawnRequest::new(params.project_dir.clone());
    req.extra_args = params.args.iter().map(std::ffi::OsString::from).collect();
    req.prompt = params.prompt;
    req.stdin_mode = StdinMode::Pty;

    // WEK-27: pass worktree options through to the manager. The flag
    // turns into a `WorktreeSpawnOptions { repo_root }` rooted at the
    // project dir; if the manager wasn't configured with a worktree
    // root we surface that as `Internal` (it's a daemon mis-wire, not
    // a user error). The manager handles the rest atomically.
    let worktree_opts = if params.worktree {
        Some(la_core::manager::WorktreeSpawnOptions {
            repo_root: std::path::PathBuf::from(&params.project_dir),
        })
    } else {
        None
    };

    let spawn_started = std::time::Instant::now();
    let spawned = state
        .manager
        .spawn_with_options(&*adapter, project_id, req, worktree_opts)
        .await
        .map_err(core_to_rpc)?;
    metrics::histogram!(
        "lad_pty_spawn_duration_seconds",
        "backend" => params.backend.clone(),
    )
    .record(spawn_started.elapsed().as_secs_f64());

    let initial_pty = ProtoPtySize {
        rows: 32,
        cols: 120,
    };
    let cwd = spawned
        .worktree_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| params.project_dir.clone());
    ok(SessionsCreateResult {
        session_id: spawned.id.0.clone(),
        backend: spawned.backend,
        cwd,
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

async fn handle_projects_list(state: &ConnState) -> Result<serde_json::Value, RpcError> {
    let storage = state.manager.storage();
    let projects = storage.projects().list().await.map_err(storage_to_rpc)?;
    let projects = projects
        .into_iter()
        .map(|p| la_proto::methods::ProjectSummary {
            project_id: p.id,
            display_name: p.display_name,
            root_path: p.root_path,
        })
        .collect();
    ok(la_proto::methods::ProjectsListResult { projects })
}

async fn handle_projects_create(
    state: &ConnState,
    params: la_proto::methods::ProjectsCreateParams,
) -> Result<serde_json::Value, RpcError> {
    let path = params.path.trim();
    if path.is_empty() {
        return Err(RpcError::new(
            error_codes::INVALID_PARAMS,
            "project path is required",
        ));
    }
    let path_buf = std::path::Path::new(path);
    if !path_buf.is_absolute() {
        return Err(RpcError::new(
            error_codes::INVALID_PARAMS,
            format!("project path must be absolute (was {path:?})"),
        ));
    }
    if !path_buf.is_dir() {
        return Err(RpcError::new(
            error_codes::INVALID_PARAMS,
            format!("{path} does not exist or is not a directory"),
        ));
    }
    // Get-or-create by root path — `ensure_project` already does exactly
    // this, so an existing project for `path` is returned idempotently.
    let project_id = ensure_project(state, path).await?;
    let storage = state.manager.storage();
    let project = storage
        .projects()
        .get(&project_id)
        .await
        .map_err(storage_to_rpc)?
        .ok_or_else(|| {
            RpcError::new(
                error_codes::INTERNAL_ERROR,
                "project vanished immediately after create",
            )
        })?;
    ok(la_proto::methods::ProjectsCreateResult {
        project: la_proto::methods::ProjectSummary {
            project_id: project.id,
            display_name: project.display_name,
            root_path: project.root_path,
        },
    })
}

async fn handle_sessions_attach(
    state: &ConnState,
    params: SessionsAttachParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id.clone());
    // `resume_from_seq` is the architecture §3 "重连一次 RPC 即可" path. The
    // semantics mirror `OutputHub::subscribe`:
    //   * `None`            ⇒ start fresh / live-only, no catch-up replay.
    //   * `Some(prev_seq)`  ⇒ replay only ring chunks with `seq > prev_seq`,
    //                          then continue live.
    // First-time attachers that want the full ring content can pass `Some(0)`.
    let outcome = state
        .manager
        .attach(&id, params.resume_from_seq, params.acquire_input)
        .await
        .map_err(core_to_rpc)?;

    // A9 (M4.5 / WEK-75): cache the session's backend so
    // `drain_subscription` can label `lad_session_output_bytes_total{backend}`
    // without re-querying storage on every chunk. Missing row falls back to
    // `"unknown"` rather than refusing the attach — losing per-backend
    // attribution on a half-state row is preferable to dropping the
    // attachment.
    let backend = state
        .manager
        .storage()
        .sessions()
        .get(&params.session_id)
        .await
        .ok()
        .flatten()
        .map(|row| row.backend_id)
        .unwrap_or_else(|| "unknown".to_string());

    let sub_id = outcome.subscription.id();
    {
        let mut attachments = state.attachments.write().await;
        attachments.insert(
            id,
            AttachmentSlot {
                sub: Some(outcome.subscription),
                sub_id,
                backend,
            },
        );
    }
    state.attachments_changed.notify_one();

    ok(SessionsAttachResult {
        session_id: params.session_id,
        snapshot_seq: outcome.snapshot_seq,
        input_acquired: outcome.input_acquired,
        // Reserved field. M1.x daemon does not mint opaque sub_tokens — the
        // client rebinds with `resume_from_seq` directly, which is sufficient
        // for the single-daemon transport. See la-proto SessionsAttachResult.
        sub_token: None,
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

async fn handle_sessions_resize(
    state: &ConnState,
    params: la_proto::methods::SessionsResizeParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id);
    state
        .manager
        .resize(&id, params.cols, params.rows)
        .await
        .map_err(core_to_rpc)?;
    ok(la_proto::methods::SessionsResizeResult {})
}

async fn handle_adapters_discover(
    state: &ConnState,
    ctx: &ConnectionContext,
    params: AdaptersDiscoverParams,
) -> Result<serde_json::Value, RpcError> {
    let mut out: Vec<ProtoDiscoveredSession> = Vec::new();

    // `source_path` is per-adapter — every backend has its own on-disk
    // layout, so transparently forwarding one override to every
    // registered adapter would have unpredictable semantics. Require
    // the caller to also pin `backend` when overriding the root.
    if params.source_path.is_some() && params.backend.is_none() {
        return Err(RpcError::invalid_params(
            "source_path requires backend to be set — every adapter has its own root",
        ));
    }

    let targets: Vec<(String, Arc<dyn AgentAdapter>)> = match params.backend.as_deref() {
        Some(name) => {
            let adapter = ctx.adapters.get(name).ok_or_else(|| {
                RpcError::new(
                    error_codes::ADAPTER_NOT_INSTALLED,
                    format!("{name}: no such backend registered"),
                )
            })?;
            vec![(name.to_string(), adapter)]
        }
        None => ctx.adapters.pairs(),
    };

    let hints = DiscoverHints {
        project_root: params.project_root.as_deref().map(PathBuf::from),
        source_path_override: params.source_path.as_deref().map(PathBuf::from),
    };

    for (id, adapter) in targets {
        let found = match adapter.discover(&hints).await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(backend = %id, error = %err, "discover: adapter failed");
                continue;
            }
        };
        for d in found {
            let existing = state
                .manager
                .storage()
                .sessions()
                .find_by_backend_external_id(&id, &d.external_id)
                .await
                .map_err(storage_to_rpc)?;
            out.push(adapter_to_proto_discovered(&id, d, existing.is_some()));
        }
    }

    ok(AdaptersDiscoverResult { discovered: out })
}

async fn handle_sessions_import(
    state: &ConnState,
    ctx: &ConnectionContext,
    params: SessionsImportParams,
) -> Result<serde_json::Value, RpcError> {
    let adapter = ctx.adapters.get(&params.backend).ok_or_else(|| {
        RpcError::new(
            error_codes::ADAPTER_NOT_INSTALLED,
            format!("{}: no such backend registered", params.backend),
        )
    })?;

    let hints = DiscoverHints {
        project_root: None,
        source_path_override: params.source_path.as_deref().map(PathBuf::from),
    };
    let found = adapter.discover(&hints).await.map_err(adapter_to_rpc)?;

    let wanted: Option<std::collections::HashSet<String>> = params
        .external_ids
        .as_ref()
        .map(|v| v.iter().cloned().collect());

    let mut out: Vec<ImportedSession> = Vec::new();
    let storage = state.manager.storage();

    for d in found {
        if let Some(ref keep) = wanted {
            if !keep.contains(&d.external_id) {
                continue;
            }
        }

        // Idempotency: same (backend, external_id) ⇒ same row. We
        // intentionally return the SQLite snapshot verbatim (not a
        // merge of fresh discover-time metadata) so a re-import never
        // returns a `created_at` that disagrees with `sessions.list`
        // when the backend has rewritten the on-disk payload since
        // the original import.
        if let Some(existing) = storage
            .sessions()
            .find_by_backend_external_id(&params.backend, &d.external_id)
            .await
            .map_err(storage_to_rpc)?
        {
            out.push(ImportedSession {
                session_id: existing.id,
                external_id: d.external_id.clone(),
                backend: params.backend.clone(),
                project_hint: d.project_hint.as_ref().map(|p| p.display().to_string()),
                created_at: existing.created_at,
                title_hint: existing.title.or_else(|| d.title_hint.clone()),
                external_path: existing.external_path,
                already_existed: true,
            });
            continue;
        }

        // Sessions need a project (FK constraint). Reuse the
        // backend's `project_hint` when present; otherwise mint a
        // synthetic per-(backend, external_id) project root using the
        // `__discovered__/...` sentinel scheme. Encoding the backend
        // + external id in the path keeps each orphan import on its
        // own row so the sidebar doesn't end up with a single
        // "unknown" bucket that grows forever, and the leading
        // sentinel is clearly not a real cwd so a future "open in
        // shell" affordance won't try to `cd` into it. Schema-level
        // NULL project_id is the cleaner long-term move (tracked as
        // follow-up before M2.4 resume lands).
        let project_dir = match d.project_hint.as_ref() {
            Some(p) => p.display().to_string(),
            None => format!("__discovered__/{}/{}", params.backend, d.external_id),
        };
        let project_id = ensure_project(state, &project_dir).await?;

        let id = la_storage::new_id();
        let external_path_str = d.external_path.as_ref().map(|p| p.display().to_string());

        let new_row = la_storage::NewSession {
            id: id.clone(),
            project_id,
            backend_id: params.backend.clone(),
            external_id: Some(d.external_id.clone()),
            title: d.title_hint.clone(),
            // Imported sessions describe past work the backend already
            // finished; they enter the table in the terminal `Exited`
            // state so the lifecycle reaper never tries to babysit a
            // process it never spawned. `resume` (M2.4+) re-spawns the
            // backend with `--resume <external_id>` against a brand new
            // PTY when the user enters the row.
            state: "exited".to_string(),
            pid: None,
            worktree_path: None,
            worktree_branch: None,
            base_branch: None,
            spawn_args: serde_json::json!({}),
            origin: "import".to_string(),
            post_create_hook_status: None,
            external_path: external_path_str.clone(),
        };

        storage
            .sessions()
            .create(new_row)
            .await
            .map_err(storage_to_rpc)?;

        out.push(ImportedSession {
            session_id: id,
            external_id: d.external_id.clone(),
            backend: params.backend.clone(),
            project_hint: d.project_hint.as_ref().map(|p| p.display().to_string()),
            created_at: d
                .created_at
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
            title_hint: d.title_hint.clone(),
            external_path: external_path_str,
            already_existed: false,
        });
    }

    ok(SessionsImportResult { imported: out })
}

fn adapter_to_proto_discovered(
    backend: &str,
    d: AdapterDiscoveredSession,
    already_imported: bool,
) -> ProtoDiscoveredSession {
    ProtoDiscoveredSession {
        backend: backend.to_string(),
        external_id: d.external_id,
        external_path: d.external_path.map(|p| p.display().to_string()),
        project_hint: d.project_hint.map(|p| p.display().to_string()),
        title_hint: d.title_hint,
        created_at: d.created_at,
        already_imported,
    }
}

fn adapter_to_rpc(err: AdapterError) -> RpcError {
    match err {
        AdapterError::NotInstalled { hint } => {
            RpcError::new(error_codes::ADAPTER_NOT_INSTALLED, hint)
        }
        AdapterError::Unauthenticated { docs_url } => RpcError::new(
            error_codes::ADAPTER_UNAUTHENTICATED,
            format!("not authenticated; see {docs_url}"),
        ),
        AdapterError::SpawnFailed(io) => {
            RpcError::new(error_codes::ADAPTER_SPAWN_FAILED, io.to_string())
        }
        AdapterError::UnsupportedOption { name } => RpcError::new(
            error_codes::ADAPTER_UNSUPPORTED_OPTION,
            format!("unsupported option: {name}"),
        ),
        AdapterError::ProtocolDrift { detail } => {
            RpcError::new(error_codes::ADAPTER_PROTOCOL_DRIFT, detail)
        }
        AdapterError::Transient(detail) => RpcError::new(error_codes::ADAPTER_SPAWN_FAILED, detail),
    }
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

/// Map a cached [`la_adapter::ProbeResult`] onto the right business-code
/// RPC error for the `sessions.create` pre-flight check.
///
/// Only the variants for which `health::is_blocking` returns `true`
/// reach here in production; the unreachable arms are written defensively
/// so a future refactor (e.g. blocking on `Error` too) won't silently
/// degrade to `INTERNAL_ERROR`.
fn probe_to_rpc(backend: &str, probe: &la_adapter::ProbeResult) -> RpcError {
    use la_adapter::ProbeResult as P;
    match probe {
        P::NotInstalled { hint } => RpcError::new(
            error_codes::ADAPTER_NOT_INSTALLED,
            format!("{backend}: {hint}"),
        ),
        P::Unauthenticated { docs_url } => RpcError::new(
            error_codes::ADAPTER_UNAUTHENTICATED,
            format!("{backend}: not authenticated; see {docs_url}"),
        ),
        P::Error { detail } => RpcError::new(
            error_codes::ADAPTER_PROTOCOL_DRIFT,
            format!("{backend}: {detail}"),
        ),
        P::Available { .. } => RpcError::new(
            error_codes::INTERNAL_ERROR,
            format!("{backend}: probe_to_rpc called for Available — bug"),
        ),
    }
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

// ===========================================================================
// Worktree diff review handlers (M2.5 / WEK-28).
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeMutationKind {
    Stage,
    Unstage,
    Discard,
}

impl WorktreeMutationKind {
    fn as_str(self) -> &'static str {
        match self {
            WorktreeMutationKind::Stage => "stage",
            WorktreeMutationKind::Unstage => "unstage",
            WorktreeMutationKind::Discard => "discard",
        }
    }
}

async fn handle_worktree_status(
    state: &ConnState,
    params: WorktreeStatusParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id.clone());
    let (engine, recorded_base) = state
        .manager
        .diff_engine_for(&id)
        .await
        .ok_or_else(|| worktree_unavailable(&params.session_id))?;
    let snap = engine.status().await.map_err(core_to_rpc)?;
    let now_rfc3339 = rfc3339_now();
    let files = snap
        .files
        .into_iter()
        .map(core_to_proto_file_entry)
        .collect();
    ok(WorktreeStatusResult {
        branch: snap.branch,
        base_branch: snap.base_branch.or(recorded_base),
        head: snap.head,
        ahead: snap.ahead,
        behind: snap.behind,
        files,
        generated_at: now_rfc3339,
    })
}

async fn handle_worktree_diff(
    state: &ConnState,
    params: WorktreeDiffParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id.clone());
    let (engine, _base) = state
        .manager
        .diff_engine_for(&id)
        .await
        .ok_or_else(|| worktree_unavailable(&params.session_id))?;
    let out = engine
        .diff_file(&params.path, params.staged, params.context_lines)
        .await
        .map_err(core_to_rpc)?;
    let file = core_to_proto_file_entry(out.file);
    let hunks = out.hunks.into_iter().map(core_to_proto_hunk).collect();
    let truncated = out.truncated.map(|t| la_proto::methods::TruncationMarker {
        reason: t.reason.to_string(),
        size_bytes: t.size_bytes,
        hint: t.hint.to_string(),
    });
    ok(WorktreeDiffResult {
        file,
        hunks,
        truncated,
    })
}

async fn handle_worktree_mutation(
    state: &ConnState,
    params: WorktreeMutationParams,
    kind: WorktreeMutationKind,
) -> Result<serde_json::Value, RpcError> {
    if matches!(kind, WorktreeMutationKind::Discard) && !params.confirmed {
        return Err(core_to_rpc(la_core::CoreError::WorktreeDiscardUnconfirmed));
    }
    let id = SessionId(params.session_id.clone());
    let (engine, _base) = state
        .manager
        .diff_engine_for(&id)
        .await
        .ok_or_else(|| worktree_unavailable(&params.session_id))?;
    let outcome = match kind {
        WorktreeMutationKind::Stage => engine.stage(&params.hunk_ids).await,
        WorktreeMutationKind::Unstage => engine.unstage(&params.hunk_ids).await,
        WorktreeMutationKind::Discard => engine.discard(&params.hunk_ids).await,
    }
    .map_err(core_to_rpc)?;

    // Fire the worktree.changed broadcast — only if something actually
    // moved, so a TUI that wired an empty button click doesn't ripple
    // out to other clients.
    if !outcome.applied.is_empty() || !outcome.affected_files.is_empty() {
        let bus_event = BusEvent::WorktreeChanged(WorktreeChangedParams {
            session_id: params.session_id.clone(),
            kind: kind.as_str().to_string(),
            affected_files: outcome.affected_files.clone(),
            generated_at: rfc3339_now(),
        });
        state.manager.bus().publish(bus_event);
    }

    let proto = WorktreeMutationResult {
        applied: outcome.applied,
        rejected: outcome
            .rejected
            .into_iter()
            .map(|r| la_proto::methods::HunkReject {
                hunk_id: r.hunk_id,
                reason: r.reason.to_string(),
            })
            .collect(),
        status: outcome
            .status
            .into_iter()
            .map(core_to_proto_file_entry)
            .collect(),
    };
    ok(proto)
}

async fn handle_worktree_commit(
    state: &ConnState,
    params: WorktreeCommitParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id.clone());
    let (engine, _base) = state
        .manager
        .diff_engine_for(&id)
        .await
        .ok_or_else(|| worktree_unavailable(&params.session_id))?;
    let outcome = engine
        .commit(&params.message, params.allow_empty)
        .await
        .map_err(core_to_rpc)?;

    let bus = state.manager.bus();
    bus.publish(BusEvent::WorktreeChanged(WorktreeChangedParams {
        session_id: params.session_id.clone(),
        kind: "commit".to_string(),
        affected_files: vec![],
        generated_at: rfc3339_now(),
    }));
    bus.publish(BusEvent::WorktreeCommitCreated(
        WorktreeCommitCreatedParams {
            session_id: params.session_id.clone(),
            commit_sha: outcome.commit_sha.clone(),
            summary: outcome.summary.clone(),
            files_changed: outcome.files_changed,
            generated_at: rfc3339_now(),
        },
    ));

    ok(WorktreeCommitResult {
        commit_sha: outcome.commit_sha,
        summary: outcome.summary,
        files_changed: outcome.files_changed,
    })
}

async fn handle_worktree_open_in_editor(
    state: &ConnState,
    params: WorktreeOpenInEditorParams,
) -> Result<serde_json::Value, RpcError> {
    let id = SessionId(params.session_id.clone());
    let (engine, _base) = state
        .manager
        .diff_engine_for(&id)
        .await
        .ok_or_else(|| worktree_unavailable(&params.session_id))?;
    let outcome = engine
        .open_in_editor(
            &params.path,
            params.line,
            params.column,
            params.editor_override.as_deref(),
        )
        .await
        .map_err(core_to_rpc)?;
    ok(WorktreeOpenInEditorResult {
        launched: outcome.launched,
        command: outcome.command,
        pid: outcome.pid,
    })
}

fn worktree_unavailable(session_id: &str) -> RpcError {
    let kind = la_proto::ErrorKind::WorktreeUnavailable;
    RpcError::new(
        kind.code(),
        format!(
            "worktree unavailable for session {session_id}: \
             session has no `worktree_path` (or the id is unknown)"
        ),
    )
}

fn rfc3339_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Inline conversion so we don't add a new chrono dep just for one
    // timestamp. Same approach the rest of the daemon uses.
    let (year, month, day, hour, min, sec) = unix_to_ymd_hms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn unix_to_ymd_hms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let total_days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let hour = (secs_of_day / 3_600) as u32;
    let min = ((secs_of_day % 3_600) / 60) as u32;
    let sec = (secs_of_day % 60) as u32;
    // Algorithm from Howard Hinnant's date library — civil_from_days.
    let z = total_days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d, hour, min, sec)
}

fn core_to_proto_file_entry(f: la_core::FileEntry) -> la_proto::methods::FileEntry {
    use la_proto::methods::{FileKind as PK, FileStatus as PS, ModeChange};
    let status = match f.status {
        la_core::FileStatus::Added => PS::Added,
        la_core::FileStatus::Modified => PS::Modified,
        la_core::FileStatus::Deleted => PS::Deleted,
        la_core::FileStatus::Renamed => PS::Renamed,
        la_core::FileStatus::Copied => PS::Copied,
        la_core::FileStatus::Untracked => PS::Untracked,
        la_core::FileStatus::Conflicted => PS::Conflicted,
    };
    let kind = match f.kind {
        la_core::FileKind::Text => PK::Text,
        la_core::FileKind::Binary => PK::Binary,
        la_core::FileKind::Submodule => PK::Submodule,
        la_core::FileKind::Symlink => PK::Symlink,
    };
    la_proto::methods::FileEntry {
        path: f.path,
        old_path: f.old_path,
        status,
        kind,
        staged_hunks: f.staged_hunks,
        unstaged_hunks: f.unstaged_hunks,
        size_bytes: f.size_bytes,
        mode_change: f.mode_change.map(|(o, n)| ModeChange {
            old_mode: o,
            new_mode: n,
        }),
    }
}

fn core_to_proto_hunk(h: la_core::Hunk) -> la_proto::methods::Hunk {
    use la_proto::methods::{DiffLine, DiffOrigin, LineRange};
    let lines = h
        .lines
        .into_iter()
        .map(|l| DiffLine {
            origin: match l.origin {
                la_core::LineOrigin::Context => DiffOrigin::Context,
                la_core::LineOrigin::Add => DiffOrigin::Add,
                la_core::LineOrigin::Delete => DiffOrigin::Delete,
            },
            content: l.content,
            no_newline: l.no_newline,
        })
        .collect();
    la_proto::methods::Hunk {
        hunk_id: h.hunk_id,
        staged: h.staged,
        old_range: LineRange {
            start: h.old_start,
            count: h.old_count,
        },
        new_range: LineRange {
            start: h.new_start,
            count: h.new_count,
        },
        header: h.header,
        lines,
    }
}

// ===========================================================================
// Cron + runs handlers (M3.9 / WEK-57).
//
// Mutations route through `crate::scheduler::SchedulerServices` so the
// in-memory heap and the SQLite row move atomically. Reads go straight at
// the repos. Errors map onto the `CRON_*` and `STORAGE_*` codes via
// `cron_op_to_rpc`.
// ===========================================================================

use la_proto::methods::{
    CronEnableConfirmation, CronEntry, CronsDeleteParams, CronsDeleteResult, CronsDryRunParams,
    CronsDryRunResult, CronsGetParams, CronsGetResult, CronsListParams, CronsListResult,
    CronsRunNowParams, CronsRunNowResult, CronsSetEnabledParams, CronsSetEnabledResult,
    CronsUpsertParams, CronsUpsertResult, RunEntry, RunsGetParams, RunsGetResult, RunsListParams,
    RunsListResult,
};

fn cron_op_to_rpc(err: crate::scheduler::CronOpError) -> RpcError {
    use crate::scheduler::CronOpError as E;
    match err {
        E::NotFound(id) => {
            RpcError::new(error_codes::CRON_NOT_FOUND, format!("cron {id} not found"))
        }
        E::InvalidExpr(reason) => RpcError::new(error_codes::CRON_INVALID_EXPR, reason),
        E::InvalidTz(tz) => RpcError::new(
            error_codes::CRON_INVALID_TZ,
            format!("invalid timezone: {tz}"),
        ),
        E::Storage(e) => storage_to_rpc(e),
        E::Other(s) => RpcError::new(error_codes::INTERNAL_ERROR, s),
    }
}

fn cron_control_to_rpc(err: crate::cron_control::CronControlError) -> RpcError {
    use crate::cron_control::CronControlError as E;
    use crate::cron_security::CronSecurityError as SecErr;
    match err {
        E::NotFound(id) => {
            RpcError::new(error_codes::CRON_NOT_FOUND, format!("cron {id} not found"))
        }
        E::InvalidExpr(reason) => RpcError::new(error_codes::CRON_INVALID_EXPR, reason),
        E::InvalidTz(tz) => RpcError::new(
            error_codes::CRON_INVALID_TZ,
            format!("invalid timezone: {tz}"),
        ),
        E::Storage(e) => storage_to_rpc(e),
        E::Security(SecErr::PromptTooLarge { actual, limit }) => RpcError::new(
            error_codes::CRON_PROMPT_TOO_LARGE,
            format!("prompt is {actual} bytes; max is {limit}"),
        ),
        E::Security(SecErr::InvalidConfirmationToken) => RpcError::new(
            error_codes::CRON_CONFIRMATION_REQUIRED,
            "invalid confirmation token; restart the two-step enable flow",
        ),
        E::Security(SecErr::ExpiredConfirmationToken) => RpcError::new(
            error_codes::CRON_CONFIRMATION_REQUIRED,
            "confirmation token expired; restart the two-step enable flow",
        ),
        E::Security(SecErr::TokenCronMismatch) => RpcError::new(
            error_codes::CRON_CONFIRMATION_REQUIRED,
            "confirmation token does not belong to this cron",
        ),
        E::Security(SecErr::SensitiveSnapshotChanged) => RpcError::new(
            error_codes::CRON_CONFIRMATION_REQUIRED,
            "cron sensitive content changed since this token was issued; \
             restart the two-step enable flow",
        ),
        E::Security(SecErr::RandomToken) => RpcError::new(
            error_codes::INTERNAL_ERROR,
            "failed to generate confirmation token",
        ),
        E::Other(s) => RpcError::new(error_codes::INTERNAL_ERROR, s),
    }
}

async fn handle_crons_list(
    state: &ConnState,
    params: CronsListParams,
) -> Result<serde_json::Value, RpcError> {
    let storage = state.manager.storage();
    let all = storage.crons().list().await.map_err(storage_to_rpc)?;
    let crons: Vec<CronEntry> = all
        .into_iter()
        .filter(|c| {
            if let Some(p) = params.project_id.as_deref() {
                if c.project_id != p {
                    return false;
                }
            }
            params.include_disabled || c.enabled != 0
        })
        .map(crate::scheduler::cron_to_wire)
        .collect();
    ok(CronsListResult { crons })
}

async fn handle_crons_get(
    state: &ConnState,
    params: CronsGetParams,
) -> Result<serde_json::Value, RpcError> {
    let storage = state.manager.storage();
    let cron = storage
        .crons()
        .get(&params.cron_id)
        .await
        .map_err(storage_to_rpc)?
        .ok_or_else(|| {
            RpcError::new(
                error_codes::CRON_NOT_FOUND,
                format!("cron {} not found", params.cron_id),
            )
        })?;
    ok(CronsGetResult {
        cron: crate::scheduler::cron_to_wire(cron),
    })
}

async fn handle_crons_upsert(
    state: &ConnState,
    ctx: &ConnectionContext,
    params: CronsUpsertParams,
) -> Result<serde_json::Value, RpcError> {
    // Reject unknown backend up-front for a clean wire error.
    if ctx.adapters.get(&params.backend).is_none() {
        return Err(RpcError::new(
            error_codes::ADAPTER_NOT_INSTALLED,
            format!("backend {:?} not registered", params.backend),
        ));
    }
    let id = params.id.unwrap_or_else(la_storage::new_id);
    // Preserve existing counter / last_fired_at on update so a TUI tweak
    // doesn't reset the cron's history. The control channel will replace
    // `enabled` according to the security decision; the value we put here
    // is intentionally ignored by `CronControl::upsert`.
    let existing = state
        .manager
        .storage()
        .crons()
        .get(&id)
        .await
        .map_err(storage_to_rpc)?;
    let consecutive_failures = existing
        .as_ref()
        .map(|c| c.consecutive_failures)
        .unwrap_or(0);
    let last_fired_at = existing.as_ref().and_then(|c| c.last_fired_at.clone());

    let upsert = la_storage::CronUpsert {
        id,
        name: params.name,
        // Carried by CronControl::upsert; it overwrites this from the
        // security decision (new crons stay disabled; sensitive-field
        // edits force-disable; non-sensitive edits preserve).
        enabled: false,
        project_id: params.project_id,
        backend_id: params.backend,
        spawn_args: params.spawn_args,
        prompt: params.prompt,
        cron_expr: params.cron_expr,
        tz: params.tz,
        catchup_mode: params.catchup_mode,
        max_concurrent_runs: params.max_concurrent_runs as i64,
        max_runs_per_day: params.max_runs_per_day as i64,
        max_runtime_s: params.max_runtime_s as i64,
        cost_budget_usd_per_day: params.cost_budget_usd_per_day,
        failure_backoff: params.failure_backoff,
        pause_on_consecutive_failures: params.pause_on_consecutive_failures as i64,
        consecutive_failures,
        last_fired_at,
        next_fire_at: None,
    };
    let outcome = ctx
        .scheduler
        .control
        .upsert(upsert)
        .await
        .map_err(cron_control_to_rpc)?;
    ok(CronsUpsertResult {
        cron: crate::scheduler::cron_to_wire(outcome.cron),
        requires_reconfirmation: outcome.requires_reconfirmation,
    })
}

async fn handle_crons_delete(
    state: &ConnState,
    ctx: &ConnectionContext,
    params: CronsDeleteParams,
) -> Result<serde_json::Value, RpcError> {
    let _ = state; // storage access lives behind ctx.scheduler.control
    let deleted = ctx
        .scheduler
        .control
        .delete(&params.cron_id)
        .await
        .map_err(cron_control_to_rpc)?;
    ok(CronsDeleteResult { deleted })
}

async fn handle_crons_set_enabled(
    state: &ConnState,
    ctx: &ConnectionContext,
    params: CronsSetEnabledParams,
) -> Result<serde_json::Value, RpcError> {
    let _ = state;
    let now = std::time::Instant::now();
    let outcome = ctx
        .scheduler
        .control
        .set_enabled(
            &params.cron_id,
            params.enabled,
            params.confirmation_token.as_deref(),
            now,
        )
        .await
        .map_err(cron_control_to_rpc)?;
    match outcome {
        crate::cron_control::SetEnabledOutcome::Applied { cron } => ok(CronsSetEnabledResult {
            cron: crate::scheduler::cron_to_wire(cron),
            requires_confirmation: None,
        }),
        crate::cron_control::SetEnabledOutcome::RequiresConfirmation {
            cron,
            token,
            summary,
        } => ok(CronsSetEnabledResult {
            cron: crate::scheduler::cron_to_wire(cron),
            requires_confirmation: Some(CronEnableConfirmation {
                confirmation_token: token.expose_secret().to_string(),
                prompt_preview: summary.prompt_preview,
                next_fire_at: summary.next_fire_at,
                daily_cost_budget: summary.budget,
            }),
        }),
    }
}

async fn handle_crons_run_now(
    state: &ConnState,
    ctx: &ConnectionContext,
    params: CronsRunNowParams,
) -> Result<serde_json::Value, RpcError> {
    let outcome = crate::scheduler::run_now(
        &ctx.scheduler,
        state.manager.storage(),
        &ctx.adapters,
        &state.manager,
        &params.cron_id,
    )
    .await
    .map_err(cron_op_to_rpc)?;
    match outcome {
        crate::scheduler::RunNowOutcome::Admitted { run_id } => ok(CronsRunNowResult {
            admitted: true,
            run_id: Some(run_id),
            refused: None,
        }),
        crate::scheduler::RunNowOutcome::Refused { reason, .. } => ok(CronsRunNowResult {
            admitted: false,
            run_id: None,
            refused: Some(reason),
        }),
    }
}

fn handle_crons_dry_run(params: CronsDryRunParams) -> Result<serde_json::Value, RpcError> {
    let count = params.count.clamp(1, 20);
    let fires = crate::scheduler::dry_run_fires(&params.cron_expr, &params.tz, count)
        .map_err(cron_op_to_rpc)?;
    let fires = fires.into_iter().map(|dt| dt.to_rfc3339()).collect();
    ok(CronsDryRunResult { fires })
}

async fn handle_runs_list(
    state: &ConnState,
    params: RunsListParams,
) -> Result<serde_json::Value, RpcError> {
    let storage = state.manager.storage();
    // RFC3339 → SQLite-lexical conversion. If the caller already provided
    // SQLite-lexical we pass it through unchanged.
    let since_lex = params.since.as_deref().map(rfc3339_to_sqlite_lexical);
    let limit = params.limit.clamp(1, 500) as i64;
    let filter = la_storage::RunsListFilter {
        cron_id: params.cron_id.as_deref(),
        since: since_lex.as_deref(),
        limit,
    };
    let rows = storage.runs().list(filter).await.map_err(storage_to_rpc)?;
    let runs: Vec<RunEntry> = rows
        .into_iter()
        .map(crate::scheduler::run_to_wire)
        .collect();
    ok(RunsListResult { runs })
}

async fn handle_runs_get(
    state: &ConnState,
    params: RunsGetParams,
) -> Result<serde_json::Value, RpcError> {
    let storage = state.manager.storage();
    let row = storage
        .runs()
        .get(&params.run_id)
        .await
        .map_err(storage_to_rpc)?
        .ok_or_else(|| {
            RpcError::new(
                error_codes::SESSION_NOT_FOUND,
                format!("run {} not found", params.run_id),
            )
        })?;
    ok(RunsGetResult {
        run: crate::scheduler::run_to_wire(row),
    })
}

fn rfc3339_to_sqlite_lexical(s: &str) -> String {
    // Try parsing as RFC3339; on failure assume the caller passed the
    // SQLite lexical form already and pass it through.
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        Err(_) => s.to_string(),
    }
}
