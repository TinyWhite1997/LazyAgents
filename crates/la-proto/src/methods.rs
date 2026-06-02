//! Typed parameters and results for the daemon ↔ client RPC surface.
//!
//! M1.1 extends the earlier M0.2 minimum set (`initialize`, `sessions.create`,
//! `sessions.attach`, `sessions.write`, plus the `session.output`
//! notification) to cover the full `sessions.*` / `events.subscribe` table
//! in architecture §3. Cron and run history (`crons.*`, `runs.*`) are
//! still out of scope until M3 and are NOT defined here.
//!
//! Each method type implements [`Method`], which centralizes the method-name
//! string so the dispatcher and the schema-export binary stay in lock-step
//! with the type. [`METHOD_NAMES`] enumerates every wire method this crate
//! knows about (in handshake-friendly order).
//!
//! ### Backwards compatibility rule
//!
//! New fields added across the M1 minor are non-breaking ONLY because every
//! optional field on every params struct carries `#[serde(default)]`. Adding
//! a required field is a wire break and requires bumping
//! [`crate::PROTOCOL_VERSION`]. Tests in `tests/round_trip.rs` deserialize
//! older-shaped payloads to catch accidental breaks.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// All RPC methods defined by `la-proto`, in the order a fresh client is
/// expected to call them.
///
/// Used by the dispatcher and by `gen_schema` to enumerate types. Order
/// matters for documentation only — at runtime any method may arrive first
/// after `initialize`.
pub const METHOD_NAMES: &[&str] = &[
    Initialize::NAME,
    Shutdown::NAME,
    SessionsList::NAME,
    SessionsCreate::NAME,
    SessionsAttach::NAME,
    SessionsDetach::NAME,
    SessionsWrite::NAME,
    SessionsResize::NAME,
    SessionsSignal::NAME,
    SessionsArchive::NAME,
    SessionsDelete::NAME,
    AdaptersDiscover::NAME,
    SessionsImport::NAME,
    SessionsReplay::NAME,
    EventsSubscribe::NAME,
];

/// Trait carried by every RPC method type for static method-name lookup.
///
/// `Params` and `Result` are the wire types; the constant `NAME` is the
/// JSON-RPC method string.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize + for<'de> Deserialize<'de> + JsonSchema;
    type Result: Serialize + for<'de> Deserialize<'de> + JsonSchema;
}

// ---------- initialize ----------

/// Connection handshake — must be the first call on every new connection.
///
/// The client advertises which protocol majors it understands; the daemon
/// echoes back the chosen one. If none overlap, the daemon SHOULD reply with
/// [`crate::error_codes::UNSUPPORTED_PROTOCOL_VERSION`] and close the
/// connection (transport behaviour, not enforced by this crate).
pub enum Initialize {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "InitializeParams")]
pub struct InitializeParams {
    /// Short client identifier, e.g. `"la"`.
    pub client: String,
    /// Semver of the client binary, e.g. `"0.4.1"`.
    pub client_version: String,
    /// Ordered list of protocol majors the client supports.
    /// The daemon picks the highest one it also supports.
    pub protocol_versions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "InitializeResult")]
pub struct InitializeResult {
    /// Daemon name; for parity with `client`. Defaults to `"lad"`.
    pub server: String,
    /// Semver of the daemon binary.
    pub server_version: String,
    /// The single protocol version negotiated for this connection.
    pub protocol_version: String,
    /// Server-advertised capabilities. Fields are additive across minors;
    /// unknown ones must be ignored by clients (architecture §3 版本策略).
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
pub struct ServerCapabilities {
    /// Names of adapters the daemon can spawn (e.g. `["claude", "codex"]`).
    #[serde(default)]
    pub adapters: Vec<String>,
    /// True once the cron subsystem is enabled (post-M1).
    #[serde(default)]
    pub cron: bool,
    /// True once worktree-based isolation is enabled (post-M1).
    #[serde(default)]
    pub worktree: bool,
    /// True once `events.subscribe` is honoured by the daemon (M1+).
    #[serde(default)]
    pub events: bool,
}

impl Method for Initialize {
    const NAME: &'static str = "initialize";
    type Params = InitializeParams;
    type Result = InitializeResult;
}

// ---------- shutdown ----------

/// Polite disconnect: the daemon flushes pending work for this connection
/// and closes it. The daemon process keeps running and serving other
/// clients (architecture §3: "礼貌断开（daemon 不退出）").
pub enum Shutdown {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "ShutdownParams")]
pub struct ShutdownParams {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "ShutdownResult")]
pub struct ShutdownResult {}

impl Method for Shutdown {
    const NAME: &'static str = "shutdown";
    type Params = ShutdownParams;
    type Result = ShutdownResult;
}

// ---------- sessions.list ----------

/// Enumerate known sessions, optionally filtered by project / backend /
/// archive state. Returns lightweight summaries; full detail comes from
/// `sessions.attach` (which is when the daemon actually subscribes the
/// client to the output stream).
pub enum SessionsList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsListParams")]
pub struct SessionsListParams {
    /// Restrict to a single project id (UUID v7). `None` ⇒ all projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Restrict to a single backend name (e.g. `"claude"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Include archived sessions in the result. Default `false` mirrors the
    /// TUI's primary view, which hides archives behind a toggle.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsListResult")]
pub struct SessionsListResult {
    pub sessions: Vec<SessionSummary>,
}

/// Compact session row returned by `sessions.list`. Larger / costlier fields
/// (transcript path, spawn args) live on the create/attach results instead
/// and are only returned when the client actually wants them.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    /// Owning project id (UUID v7).
    pub project_id: String,
    /// Backend name; matches one of [`ServerCapabilities::adapters`].
    pub backend: String,
    /// User-editable title; `None` until the user names the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub state: SessionState,
    /// `'user'` for a human-started session, `'cron:<cron_id>'` for a
    /// scheduled run, `'import'` for one discovered on disk.
    pub origin: String,
    /// RFC3339 timestamp the session row was created.
    pub created_at: String,
    /// RFC3339 timestamp of the last state change (output, write, exit, …).
    pub updated_at: String,
    /// Worktree path if any; `None` for sessions sharing the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
}

impl Method for SessionsList {
    const NAME: &'static str = "sessions.list";
    type Params = SessionsListParams;
    type Result = SessionsListResult;
}

// ---------- sessions.create ----------

/// Create a new agent session. Returns the session metadata; output then
/// streams as `session.output` notifications once the client `attach`es.
pub enum SessionsCreate {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsCreateParams")]
pub struct SessionsCreateParams {
    /// Absolute path of the project directory the agent should `cd` into.
    pub project_dir: String,
    /// Backend identifier — must match one of the names in
    /// [`ServerCapabilities::adapters`].
    pub backend: String,
    /// Extra args appended to the adapter's base command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Initial prompt to push to stdin after the first prompt loop is ready.
    /// Omit to leave the session waiting for human input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// If true, create a fresh git worktree for this session (post-M1; M0
    /// daemon may ignore).
    #[serde(default)]
    pub worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsCreateResult")]
pub struct SessionsCreateResult {
    /// Newly assigned session id (UUID v7).
    pub session_id: String,
    /// Backend echoed back so a multi-backend client doesn't need to remember.
    pub backend: String,
    /// Resolved working directory of the child process.
    pub cwd: String,
    /// Initial PTY size the child was spawned with.
    pub initial_size: PtySize,
    /// State the session is in when this response is returned.
    pub state: SessionState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Starting,
    Running,
    Waiting,
    Exited,
    Errored,
    /// Soft-deleted by the user via `sessions.archive`. The row stays in
    /// SQLite (and may be restored) but is hidden from the default list.
    Archived,
}

impl Method for SessionsCreate {
    const NAME: &'static str = "sessions.create";
    type Params = SessionsCreateParams;
    type Result = SessionsCreateResult;
}

// ---------- sessions.attach ----------

/// Subscribe to a session's output stream. The response includes a `snapshot_seq`:
/// any `session.output` notification with `seq <= snapshot_seq` is the
/// catch-up replay; subsequent ones are live increments. (Architecture §3.)
pub enum SessionsAttach {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsAttachParams")]
pub struct SessionsAttachParams {
    pub session_id: String,
    /// Resume cursor. The semantics match the hub-level subscription:
    /// `None` ⇒ start fresh / live-only, no catch-up replay (the response's
    /// `snapshot_seq` tells the client which `seq` the live stream starts
    /// after — typically used by first-time attachers that do not need
    /// historical output).
    /// `Some(prev_seq)` ⇒ daemon replays only ring chunks whose
    /// `seq > prev_seq`, then continues live. This is the architecture §3
    /// "重连一次 RPC 即可" path: a reconnecting client carries its last
    /// observed `seq` and gets a single `attach` that both resubscribes and
    /// catches up, with no follow-up `sessions.replay` required as long as
    /// the bytes are still in the ring.
    ///
    /// A first-time attacher that does want everything currently in the
    /// ring can pass `Some(0)` explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_from_seq: Option<u64>,
    /// Replay window: the daemon should resend at most this many bytes from
    /// its ring buffer before live streaming. `None` ⇒ daemon default.
    ///
    /// **Deprecated** (kept for wire-schema back-compat only). The daemon
    /// no longer honours this field; reconnecting clients should pass
    /// `resume_from_seq` instead, which expresses the same intent more
    /// precisely (a `seq` boundary, not a byte count). New code should
    /// omit this field. Will be removed in a future minor version once
    /// no in-the-wild client serialises it. See WEK-49.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_bytes: Option<u64>,
    /// Request input ownership. Only one writer per session at a time
    /// (architecture §3 关键不变量: 单一写者).
    #[serde(default)]
    pub acquire_input: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsAttachResult")]
pub struct SessionsAttachResult {
    pub session_id: String,
    /// Last `seq` covered by the catch-up replay; the first strictly-greater
    /// `session.output` is live.
    pub snapshot_seq: u64,
    /// Whether this client acquired input ownership (might differ from
    /// `acquire_input` if another client is already the writer).
    pub input_acquired: bool,
    /// Opaque resume token. Reserved for future use: the daemon MAY return
    /// a token here that a reconnecting client passes back so the daemon
    /// can rebind to the same parked subscription instead of issuing a
    /// fresh one. M1.x does not require the daemon to emit this — the wire
    /// layer is prepared so M1.7+ can opt in without another schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_token: Option<String>,
}

impl Method for SessionsAttach {
    const NAME: &'static str = "sessions.attach";
    type Params = SessionsAttachParams;
    type Result = SessionsAttachResult;
}

// ---------- sessions.detach ----------

/// Cancel a single connection's subscription. The session itself keeps
/// running (architecture §3: "不影响会话存活"). The daemon also auto-detaches
/// on connection close, so calling this is optional but lets a client free
/// its server-side ring-buffer slot eagerly.
pub enum SessionsDetach {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsDetachParams")]
pub struct SessionsDetachParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsDetachResult")]
pub struct SessionsDetachResult {}

impl Method for SessionsDetach {
    const NAME: &'static str = "sessions.detach";
    type Params = SessionsDetachParams;
    type Result = SessionsDetachResult;
}

// ---------- sessions.write ----------

/// Send bytes to the session's PTY master (typed keystrokes, paste, etc.).
///
/// The payload is **base64-encoded** so that arbitrary bytes (including the
/// NUL byte and control sequences) can ride over JSON without escaping
/// surprises. The decode helpers below preserve that contract.
pub enum SessionsWrite {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsWriteParams")]
pub struct SessionsWriteParams {
    pub session_id: String,
    /// Base64-encoded bytes to write to the PTY master.
    pub data_base64: String,
}

impl SessionsWriteParams {
    /// Maximum raw byte length acceptable for `data` before base64 + JSON
    /// envelope expansion is guaranteed to fit under [`crate::MAX_MESSAGE_BYTES`].
    ///
    /// base64 expands by a factor of 4/3 (rounded up to 4); the JSON envelope
    /// (`{"jsonrpc":...,"id":...,"method":"sessions.write","params":{
    /// "session_id":"...","data_base64":"..."}}`) adds a small fixed overhead.
    /// We budget 16 KiB of slack to cover the envelope + session_id of any
    /// realistic size.
    pub const MAX_RAW_BYTES: usize = (crate::MAX_MESSAGE_BYTES - 16 * 1024) / 4 * 3;

    /// Construct from raw bytes; encodes on the way in.
    ///
    /// Panics if `data.len() > Self::MAX_RAW_BYTES`. Use
    /// [`try_from_bytes`](Self::try_from_bytes) for a fallible variant.
    pub fn from_bytes(session_id: impl Into<String>, data: &[u8]) -> Self {
        Self::try_from_bytes(session_id, data).expect("payload over MAX_RAW_BYTES")
    }

    /// Fallible constructor: returns an error if `data` is too large to ever
    /// fit on the wire after base64 + JSON envelope expansion.
    pub fn try_from_bytes(
        session_id: impl Into<String>,
        data: &[u8],
    ) -> Result<Self, PayloadTooLarge> {
        if data.len() > Self::MAX_RAW_BYTES {
            return Err(PayloadTooLarge {
                actual: data.len(),
                limit: Self::MAX_RAW_BYTES,
            });
        }
        Ok(Self {
            session_id: session_id.into(),
            data_base64: B64.encode(data),
        })
    }

    /// Decode the payload bytes. Returns an [`Err`] on non-base64 input.
    pub fn data_bytes(&self) -> Result<Vec<u8>, base64::DecodeError> {
        B64.decode(self.data_base64.as_bytes())
    }
}

/// Returned by [`SessionsWriteParams::try_from_bytes`] and friends when raw
/// data would not fit under [`crate::MAX_MESSAGE_BYTES`] after encoding.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("raw payload of {actual} bytes exceeds max-on-wire size of {limit} bytes")]
pub struct PayloadTooLarge {
    pub actual: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[schemars(rename = "SessionsWriteResult")]
pub struct SessionsWriteResult {}

impl Method for SessionsWrite {
    const NAME: &'static str = "sessions.write";
    type Params = SessionsWriteParams;
    type Result = SessionsWriteResult;
}

// ---------- sessions.resize ----------

/// Adjust the PTY window size for the session (cols/rows). `portable-pty`
/// applies this consistently on ConPTY and Unix; the daemon also pushes a
/// `WINCH` so curses-based UIs in the child redraw.
pub enum SessionsResize {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsResizeParams")]
pub struct SessionsResizeParams {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsResizeResult")]
pub struct SessionsResizeResult {}

impl Method for SessionsResize {
    const NAME: &'static str = "sessions.resize";
    type Params = SessionsResizeParams;
    type Result = SessionsResizeResult;
}

// ---------- sessions.signal ----------

/// Signal-name vocabulary supported on the wire. Mapped per architecture
/// §6.3 to the platform primitive (`nix::sys::signal::kill` on Unix,
/// `GenerateConsoleCtrlEvent` / `TerminateProcess` on Windows).
///
/// Kept as a tagged enum (not a free `String`) so the dispatcher can reject
/// unknown signals at `serde` decode time, before the request reaches the
/// signal-mapping code path. Adding a signal here is a wire-level
/// compatibility decision and must be backed by an architecture update.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum SessionSignal {
    /// Equivalent of Ctrl-C: Unix SIGINT / Windows CTRL_C_EVENT.
    Int,
    /// Polite termination: Unix SIGTERM / Windows CTRL_BREAK_EVENT.
    Term,
    /// Hard kill: Unix SIGKILL / Windows TerminateProcess.
    Kill,
}

pub enum SessionsSignal {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsSignalParams")]
pub struct SessionsSignalParams {
    pub session_id: String,
    pub signal: SessionSignal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsSignalResult")]
pub struct SessionsSignalResult {}

impl Method for SessionsSignal {
    const NAME: &'static str = "sessions.signal";
    type Params = SessionsSignalParams;
    type Result = SessionsSignalResult;
}

// ---------- sessions.archive / sessions.delete ----------

/// Soft-delete: hide from the default `sessions.list` view and mark
/// `state=archived`. The row, worktree, and transcript stay on disk until
/// the prune window expires (architecture §8.4).
pub enum SessionsArchive {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsArchiveParams")]
pub struct SessionsArchiveParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsArchiveResult")]
pub struct SessionsArchiveResult {}

impl Method for SessionsArchive {
    const NAME: &'static str = "sessions.archive";
    type Params = SessionsArchiveParams;
    type Result = SessionsArchiveResult;
}

/// Hard-delete: remove the session row, cascade `session_chunks`, and
/// schedule worktree teardown. Irreversible; the daemon SHOULD refuse if
/// the session is still `running` (use `sessions.signal` first).
pub enum SessionsDelete {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsDeleteParams")]
pub struct SessionsDeleteParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsDeleteResult")]
pub struct SessionsDeleteResult {}

impl Method for SessionsDelete {
    const NAME: &'static str = "sessions.delete";
    type Params = SessionsDeleteParams;
    type Result = SessionsDeleteResult;
}

// ---------- adapters.discover ----------

/// Walk every registered adapter's on-disk session store and surface
/// what exists, without altering it (architecture §4.2 双轨发现).
///
/// The daemon returns one entry per session the adapter found on disk;
/// rows already promoted to native LazyAgents sessions (i.e. previously
/// `sessions.import`-ed) are flagged via `already_imported = true` so
/// the TUI can grey them out instead of offering import again.
///
/// `source_path` lets the client point a single adapter at a non-default
/// discovery root (e.g. a fixture dir during tests). When `backend` is
/// `None` the daemon iterates every registered adapter.
pub enum AdaptersDiscover {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[schemars(rename = "AdaptersDiscoverParams")]
pub struct AdaptersDiscoverParams {
    /// Restrict discovery to a single backend; `None` ⇒ every adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Override the adapter's default discovery root. Only meaningful
    /// when `backend` is set (each adapter has its own root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Restrict results to sessions whose recorded cwd matches this
    /// project root. `None` ⇒ no filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "AdaptersDiscoverResult")]
pub struct AdaptersDiscoverResult {
    pub discovered: Vec<DiscoveredSession>,
}

/// One pre-existing backend session surfaced from disk. Mirrors
/// `la_adapter::DiscoveredSession` plus the daemon-side bookkeeping
/// fields (`backend`, `external_path`, `already_imported`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DiscoveredSession {
    pub backend: String,
    pub external_id: String,
    /// Absolute path to the backend's own transcript file (JSONL/JSON).
    /// The daemon promises never to mutate it; the import path stores
    /// it as a read-only reference (architecture §4.2 双轨).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_path: Option<String>,
    /// Project root hint reported by the backend (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_hint: Option<String>,
    /// Backend-provided title or first-line preview (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_hint: Option<String>,
    /// RFC3339 creation timestamp recorded by the backend. Falls back
    /// to the discovery file's mtime when the backend payload doesn't
    /// carry one; `None` only when neither could be obtained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// `true` when the daemon's `sessions` table already contains a row
    /// for this `(backend, external_id)`. TUI should grey-out import
    /// affordance for these entries.
    #[serde(default)]
    pub already_imported: bool,
}

impl Method for AdaptersDiscover {
    const NAME: &'static str = "adapters.discover";
    type Params = AdaptersDiscoverParams;
    type Result = AdaptersDiscoverResult;
}

// ---------- sessions.import ----------

/// Promote one or more backend-native sessions discovered by
/// `adapters.discover` into the daemon's `sessions` table as read-only
/// references (architecture §4.2 双轨发现).
///
/// `external_ids` (when set) restricts the import to a specific subset;
/// `None` ⇒ import every session the adapter currently discovers. The
/// daemon never copies the backend's transcript file — it only records
/// the `external_id` + `external_path` so resume can re-attach the
/// backend's own data store.
///
/// `source_path` lets the client point the adapter at a non-default
/// discovery root (e.g. `~/.claude/projects` overridden during testing).
pub enum SessionsImport {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsImportParams")]
pub struct SessionsImportParams {
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Specific external ids to import; `None` ⇒ every discovered
    /// session. Unknown ids are silently dropped so a stale TUI
    /// snapshot never wedges an import call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsImportResult")]
pub struct SessionsImportResult {
    pub imported: Vec<ImportedSession>,
}

/// A read-only reference to a backend-native session promoted into the
/// daemon's `sessions` table. The daemon assigns a fresh `session_id`
/// so the rest of the system can reference it uniformly; `external_id`
/// preserves the backend's own id.
///
/// Re-importing a session that already exists (same `backend` +
/// `external_id`) is idempotent — `session_id` is the existing row's id
/// and `already_existed` is `true`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ImportedSession {
    pub session_id: String,
    pub external_id: String,
    pub backend: String,
    /// Project root hint reported by the backend (if any). Used to
    /// auto-place the session under an existing `projects` row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_hint: Option<String>,
    /// RFC3339 creation timestamp as recorded by the backend.
    pub created_at: String,
    /// Backend-provided title or first-line preview (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_hint: Option<String>,
    /// Absolute path to the backend's own transcript file. Persisted on
    /// the row so resume can re-attach the data store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_path: Option<String>,
    /// `true` when this `(backend, external_id)` already had a row and
    /// the daemon returned the existing `session_id` instead of
    /// creating a new one.
    #[serde(default)]
    pub already_existed: bool,
}

impl Method for SessionsImport {
    const NAME: &'static str = "sessions.import";
    type Params = SessionsImportParams;
    type Result = SessionsImportResult;
}

// ---------- sessions.replay ----------

/// Catch-up after a `session.gap` notification. The client passes the gap's
/// `from_seq` (inclusive) and the daemon replays whatever still survives in
/// the ring buffer as `session.output` notifications. Bytes evicted before
/// replay was requested are NOT recovered (the gap stands).
pub enum SessionsReplay {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsReplayParams")]
pub struct SessionsReplayParams {
    pub session_id: String,
    /// Inclusive starting sequence number for the replay.
    pub from_seq: u64,
    /// Optional hard cap on the bytes the daemon will resend before
    /// switching back to live streaming. `None` ⇒ daemon default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionsReplayResult")]
pub struct SessionsReplayResult {
    /// Last `seq` covered by the replay. If equal to `from_seq - 1`, the
    /// ring buffer no longer contained anything for that range.
    pub last_seq: u64,
    /// Total bytes about to be (re)delivered as `session.output`
    /// notifications. Lets the client budget UI updates.
    pub bytes_queued: u64,
}

impl Method for SessionsReplay {
    const NAME: &'static str = "sessions.replay";
    type Params = SessionsReplayParams;
    type Result = SessionsReplayResult;
}

// ---------- events.subscribe ----------

/// Topic vocabulary for [`EventsSubscribe`]. Each variant maps 1:1 to a
/// notification method name in [`crate::notifications::NOTIFICATION_NAMES`].
/// Adding a topic here is a wire-compat change; the dispatcher MUST reject
/// unknown ones at decode time so older daemons fail loudly rather than
/// silently dropping a subscription.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EventTopic {
    /// Per-session PTY output stream (`session.output`).
    SessionOutput,
    /// State transitions for any visible session (`session.state`).
    SessionState,
    /// Backpressure / drop notice for a session ring buffer (`session.gap`).
    SessionGap,
    /// Cron firing events (`cron.fired`).
    CronFired,
    /// Daemon health pulse for the status bar (`daemon.health`).
    DaemonHealth,
}

/// Subscribe to the global event stream (architecture §3). Per-session
/// output subscriptions are still done via `sessions.attach`; this method
/// covers the "status bar" topics that are not bound to a single session.
pub enum EventsSubscribe {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "EventsSubscribeParams")]
pub struct EventsSubscribeParams {
    /// Topics this client wants pushed. Empty ⇒ no-op; the daemon must
    /// still reply OK so the client can confirm the round-trip.
    pub topics: Vec<EventTopic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "EventsSubscribeResult")]
pub struct EventsSubscribeResult {
    /// Topics the daemon will actually push to this connection. The set may
    /// be a strict subset of `topics` if the daemon does not yet implement
    /// some of them (e.g. `cron_fired` before M3).
    pub topics: Vec<EventTopic>,
}

impl Method for EventsSubscribe {
    const NAME: &'static str = "events.subscribe";
    type Params = EventsSubscribeParams;
    type Result = EventsSubscribeResult;
}
