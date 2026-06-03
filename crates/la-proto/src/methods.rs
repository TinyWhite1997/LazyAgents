//! Typed parameters and results for the daemon ↔ client RPC surface.
//!
//! M1.1 extended the earlier M0.2 minimum set (`initialize`, `sessions.create`,
//! `sessions.attach`, `sessions.write`, plus the `session.output`
//! notification) to cover the full `sessions.*` / `events.subscribe` table
//! in architecture §3. M3 added cron and run history (`crons.*`, `runs.*`);
//! both surfaces are defined here and enumerated in [`METHOD_NAMES`].
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
    WorktreeStatus::NAME,
    WorktreeDiff::NAME,
    WorktreeStage::NAME,
    WorktreeUnstage::NAME,
    WorktreeDiscard::NAME,
    WorktreeCommit::NAME,
    WorktreeOpenInEditor::NAME,
    CronsList::NAME,
    CronsGet::NAME,
    CronsUpsert::NAME,
    CronsDelete::NAME,
    CronsSetEnabled::NAME,
    CronsRunNow::NAME,
    CronsDryRun::NAME,
    RunsList::NAME,
    RunsGet::NAME,
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
    /// True once the `worktree.*` diff review surface (status / diff /
    /// stage / unstage / discard / commit / open_in_editor) is wired up.
    /// Added in M2.5 (WEK-28). Old clients ignore the field; new clients
    /// hide the diff view when the daemon does not advertise this bit.
    #[serde(default)]
    pub diff: bool,
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
    /// Per-worktree mutation notice (`worktree.changed`). Pushed after
    /// `worktree.stage` / `worktree.unstage` / `worktree.discard` /
    /// `worktree.commit` succeeds, so other clients attached to the same
    /// session can invalidate cached diff state. M2.5 (WEK-28).
    WorktreeChanged,
    /// New commit landed on a session's worktree branch
    /// (`worktree.commit_created`). Carries `commit_sha` so the TUI can
    /// pop a toast. M2.5 (WEK-28).
    WorktreeCommit,
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

// ===========================================================================
// Worktree diff review surface (M2.5 / WEK-28).
//
// All methods below operate on a session that was created with
// `worktree: true`. The daemon resolves `session_id` to that session's
// `worktree_path`; if the field is `None` the call fails with
// [`crate::error_codes::WORKTREE_UNAVAILABLE`].
//
// Naming and shape match the WEK-8 brief §3.2:
// - `worktree.status` is a lightweight summary (no hunks). Used to render
//   the file list for the diff view.
// - `worktree.diff` returns one file at a time, lazy-loaded when the user
//   expands a fold. Files > 5 MiB return a [`TruncationMarker`] instead of
//   hunks so the TUI can suggest "open in editor".
// - `worktree.stage` / `worktree.unstage` / `worktree.discard` are hunk-
//   level. They take a list of [`hunk_id`](Hunk::hunk_id) fingerprints
//   computed from a previous `worktree.diff`; ids whose backing hunk has
//   shifted go to `rejected` with `reason = "stale"`. Discard requires
//   `confirmed = true` as a wire-level guard so the TUI cannot fire it
//   without going through the 二次确认 modal (PRD acceptance).
// - `worktree.commit` shells `git commit -F -` inside the worktree; the
//   message is read from stdin to avoid shell-quoting bugs. M2 explicitly
//   does NOT expose `--amend`, `--signoff`, GPG flag toggling, or auto-
//   push (brief §3.4).
// - `worktree.open_in_editor` spawns the user's editor and returns as soon
//   as `spawn()` succeeds. The TUI does NOT hand over the alternate
//   screen — this is fire-and-forget by design.
// ===========================================================================

// ---------- common diff types ----------

/// On-disk classification for a single file in a `worktree.status` result.
/// Matches the standard git porcelain v2 vocabulary minus `Unknown`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[schemars(rename = "WorktreeFileStatus")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    /// Either side of an unresolved merge conflict.
    Conflicted,
}

/// What kind of payload a file carries. The TUI renders binaries /
/// submodules differently and refuses hunk-level operations on them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[schemars(rename = "WorktreeFileKind")]
pub enum FileKind {
    Text,
    Binary,
    Submodule,
    Symlink,
}

/// Lightweight per-file summary returned by `worktree.status`. The
/// `staged_hunks` / `unstaged_hunks` counters let the TUI render fold
/// badges without a follow-up `worktree.diff` per file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeFileEntry")]
pub struct FileEntry {
    /// Path relative to the worktree root, forward-slash separated.
    pub path: String,
    /// Source path when `status == Renamed | Copied`; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub kind: FileKind,
    /// Number of hunks currently in the index for this file. `0` for
    /// purely unstaged changes.
    pub staged_hunks: u32,
    /// Number of hunks currently in the working tree but not the index.
    pub unstaged_hunks: u32,
    /// On-disk size of the working-tree copy (0 for `Deleted`). Lets the
    /// TUI render a size hint and pre-empts loading the diff on huge
    /// files.
    pub size_bytes: u64,
    /// `(old_mode, new_mode)` octal — only set when the file's mode bits
    /// changed across the diff (chmod +x). `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_change: Option<ModeChange>,
}

/// Octal mode pair carried on [`FileEntry::mode_change`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeModeChange")]
pub struct ModeChange {
    pub old_mode: u32,
    pub new_mode: u32,
}

/// `(start_line, line_count)` on either side of a hunk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeLineRange")]
pub struct LineRange {
    pub start: u32,
    pub count: u32,
}

/// Single line inside a hunk. Mirrors the unified-diff origin character.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[schemars(rename = "WorktreeDiffOrigin")]
pub enum DiffOrigin {
    /// Unchanged context line.
    Context,
    /// `+` line (added).
    Add,
    /// `-` line (removed).
    Delete,
}

/// One line inside a [`Hunk`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeDiffLine")]
pub struct DiffLine {
    pub origin: DiffOrigin,
    /// Line content without the leading origin byte and trailing `\n`.
    pub content: String,
    /// `true` when the file is missing the trailing newline at EOF and
    /// git emitted `\ No newline at end of file` after this line.
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_newline: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One hunk inside a `worktree.diff` response. `hunk_id` is a stable
/// fingerprint (brief §3.3) so stage / unstage are idempotent under
/// concurrent TUIs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeHunk")]
pub struct Hunk {
    /// Hex string, 16 chars. Computed as a sha256 of `path | old_range |
    /// hunk_body_bytes`; see [`crate::methods::compute_hunk_id`] in
    /// la-core. Stable across re-reads as long as the underlying bytes
    /// don't shift.
    pub hunk_id: String,
    /// `true` when this hunk is in the index (returned by a `staged:
    /// true` diff); `false` when it lives only in the working tree.
    pub staged: bool,
    pub old_range: LineRange,
    pub new_range: LineRange,
    /// Raw hunk header from git, e.g. `"@@ -12,7 +12,9 @@ fn foo()"`.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// Returned in place of `hunks` when the file is too large or otherwise
/// unsuitable for inline rendering.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeTruncationMarker")]
pub struct TruncationMarker {
    /// Machine-readable reason: `"too_large" | "binary" | "submodule"`.
    pub reason: String,
    pub size_bytes: u64,
    /// Hint string surfaced to the TUI: `"open_in_editor"`.
    pub hint: String,
}

/// Per-hunk rejection emitted by `worktree.stage` / `unstage` /
/// `discard` when an id no longer matches a live hunk.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeHunkReject")]
pub struct HunkReject {
    pub hunk_id: String,
    /// One of: `"stale"` (id no longer present), `"binary"` (per-hunk op
    /// not supported on binary file), `"conflict"` (file has unresolved
    /// merge markers), `"unknown"` (other classified failure).
    pub reason: String,
}

// ---------- worktree.status ----------

/// Equivalent of `git status --porcelain=v2 -z` + `git rev-parse HEAD`
/// scoped to a single session's worktree. Returns lightweight per-file
/// summaries; per-hunk content comes from `worktree.diff`. Brief §3.6
/// budget: p95 ≤ 100 ms.
pub enum WorktreeStatus {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeStatusParams")]
pub struct WorktreeStatusParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeStatusResult")]
pub struct WorktreeStatusResult {
    /// Current branch of the worktree (`la/session-<short_sid>` for
    /// daemon-provisioned worktrees).
    pub branch: String,
    /// Base branch the worktree was forked from when created. `None`
    /// when the session row no longer carries that hint (e.g. archived).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Resolved tip SHA of the worktree's branch (40-char hex). Empty
    /// string when the branch has no commits yet.
    pub head: String,
    /// Commits on `branch` not reachable from `base_branch`. `0` until
    /// the first commit; cheap proxy for "anything to merge upstream".
    pub ahead: u32,
    /// Commits on `base_branch` not reachable from `branch`. `0` for
    /// freshly-created worktrees.
    pub behind: u32,
    pub files: Vec<FileEntry>,
    /// RFC3339 timestamp the snapshot was taken. Used by the TUI to
    /// suppress redundant re-renders when nothing changed.
    pub generated_at: String,
}

impl Method for WorktreeStatus {
    const NAME: &'static str = "worktree.status";
    type Params = WorktreeStatusParams;
    type Result = WorktreeStatusResult;
}

// ---------- worktree.diff ----------

/// Per-file unified diff. Returned lazily when the TUI expands a file
/// fold (brief §3.3 — file-level lazy load is the only pagination layer;
/// no streaming notifications).
pub enum WorktreeDiff {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeDiffParams")]
pub struct WorktreeDiffParams {
    pub session_id: String,
    /// File path relative to the worktree root, forward-slash separated.
    pub path: String,
    /// `true` ⇒ diff between index and HEAD; `false` ⇒ working tree
    /// vs index.
    pub staged: bool,
    /// Number of context lines around each hunk. **M2.5 daemons
    /// ignore this field and always reply with `-U3`** — the
    /// `hunk_id` fingerprint is body-bytes-derived, and the mutation
    /// path (`worktree.stage` / `unstage` / `discard`) re-reads with
    /// `-U3` to recompute it. A different context here would silently
    /// produce ids the same daemon could not later apply. The field
    /// is kept on the wire for forward compat: a future minor that
    /// teaches the mutation path to replay the request's context can
    /// honour this without a schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeDiffResult")]
pub struct WorktreeDiffResult {
    pub file: FileEntry,
    /// Empty when [`truncated`](Self::truncated) is `Some`.
    pub hunks: Vec<Hunk>,
    /// Populated when the file exceeds the inline-diff cap (brief §3.6:
    /// 5 MiB) or is binary / a submodule. `hunks` is then empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<TruncationMarker>,
}

impl Method for WorktreeDiff {
    const NAME: &'static str = "worktree.diff";
    type Params = WorktreeDiffParams;
    type Result = WorktreeDiffResult;
}

// ---------- worktree.stage / unstage / discard ----------

/// Shared params shape for stage / unstage / discard. `hunk_ids` are the
/// fingerprints returned by an earlier `worktree.diff`. `confirmed` is
/// only meaningful for `discard` — the daemon refuses unless it is
/// `true` (PRD 撤销 二次确认 acceptance).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeMutationParams")]
pub struct WorktreeMutationParams {
    pub session_id: String,
    pub hunk_ids: Vec<String>,
    /// Required `true` for `worktree.discard`; ignored elsewhere.
    /// Modelled as `bool` (not `Option`) so the wire form is explicit;
    /// older clients that never set it default to `false` and therefore
    /// fail discard with `WORKTREE_DISCARD_UNCONFIRMED`, which is the
    /// safe outcome.
    #[serde(default)]
    pub confirmed: bool,
}

/// Shared response shape for stage / unstage / discard. `status` carries
/// the post-mutation [`FileEntry`] for every file touched so the TUI can
/// refresh badges in one round trip.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeMutationResult")]
pub struct WorktreeMutationResult {
    pub applied: Vec<String>,
    pub rejected: Vec<HunkReject>,
    /// `FileEntry` snapshot for every file referenced by the mutation
    /// (whether or not its hunk(s) succeeded). Order matches `files` in
    /// the most-recent `worktree.status`.
    pub status: Vec<FileEntry>,
}

pub enum WorktreeStage {}
impl Method for WorktreeStage {
    const NAME: &'static str = "worktree.stage";
    type Params = WorktreeMutationParams;
    type Result = WorktreeMutationResult;
}

pub enum WorktreeUnstage {}
impl Method for WorktreeUnstage {
    const NAME: &'static str = "worktree.unstage";
    type Params = WorktreeMutationParams;
    type Result = WorktreeMutationResult;
}

pub enum WorktreeDiscard {}
impl Method for WorktreeDiscard {
    const NAME: &'static str = "worktree.discard";
    type Params = WorktreeMutationParams;
    type Result = WorktreeMutationResult;
}

// ---------- worktree.commit ----------

/// Run `git commit -F -` inside the worktree, sending `message` on
/// stdin. Inherits the repo's existing `commit.gpgsign`, signing key,
/// commit template, and pre-commit hooks. M2 deliberately omits:
/// `--amend`, `--signoff`, GPG flag toggling, `--no-verify`, auto-push.
pub enum WorktreeCommit {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeCommitParams")]
pub struct WorktreeCommitParams {
    pub session_id: String,
    pub message: String,
    /// `true` ⇒ pass `--allow-empty`. Default `false`. Lets `git commit`
    /// land a no-op commit (e.g. to record a checkpoint after a revert).
    #[serde(default)]
    pub allow_empty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeCommitResult")]
pub struct WorktreeCommitResult {
    /// 40-char hex SHA of the new commit.
    pub commit_sha: String,
    /// First line of the commit message (the "subject"). Echoed so the
    /// TUI can render a toast without re-parsing the input.
    pub summary: String,
    /// Number of files in the commit. Pre-computed by `git
    /// diff-tree --name-only HEAD~..HEAD | wc -l` so the TUI doesn't
    /// need to round-trip.
    pub files_changed: u32,
}

impl Method for WorktreeCommit {
    const NAME: &'static str = "worktree.commit";
    type Params = WorktreeCommitParams;
    type Result = WorktreeCommitResult;
}

// ---------- worktree.open_in_editor ----------

/// Spawn `$VISUAL` / `$EDITOR` (or the configured override) pointed at
/// `path` inside the worktree. Returns as soon as the spawn syscall
/// succeeds — does NOT wait for the editor to exit and does NOT hand
/// over the alternate screen. Brief §3.2.
pub enum WorktreeOpenInEditor {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeOpenInEditorParams")]
pub struct WorktreeOpenInEditorParams {
    pub session_id: String,
    /// File path relative to the worktree root.
    pub path: String,
    /// 1-based line number; passed to editors that support `:line`
    /// jump syntax. `None` ⇒ open at file head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// 1-based column; only honoured by editors that support
    /// `:line:col` (VS Code, Cursor, Zed). Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// Override the daemon's editor resolution chain for this single
    /// call. Plain command name (`"code"`, `"cursor"`, `"zed"`,
    /// `"idea"`, …). `None` ⇒ use the daemon's standard chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeOpenInEditorResult")]
pub struct WorktreeOpenInEditorResult {
    /// `true` ⇒ child process was spawned. The daemon makes no claim
    /// about whether the editor actually opened the file.
    pub launched: bool,
    /// argv joined with spaces, redacted of environment values. Lets
    /// the TUI show "launched: code --goto path:12" in a toast.
    pub command: String,
    /// Child PID when available (Unix). `None` when the platform does
    /// not expose it or the daemon can't get it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl Method for WorktreeOpenInEditor {
    const NAME: &'static str = "worktree.open_in_editor";
    type Params = WorktreeOpenInEditorParams;
    type Result = WorktreeOpenInEditorResult;
}

// ===========================================================================
// Cron + runs surface (M3.9 / WEK-57).
//
// `crons.*` exposes the scheduler's cron table to the TUI Crons tab; mutations
// are funneled through the daemon's single scheduler control channel so the
// in-memory heap and the SQLite row stay synchronised. `runs.*` is read-only
// run-history reporting backed by `RunsRepo::list` / `get`.
// ===========================================================================

/// Wire-shape mirror of `la_storage::Cron` (architecture §4.4). All
/// timestamps are RFC3339 UTC strings — distinct from the SQLite lexical
/// `YYYY-MM-DD HH:MM:SS` form used internally so TUI / external tooling
/// don't have to know the storage detail.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "CronEntry")]
pub struct CronEntry {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub project_id: String,
    pub backend: String,
    /// JSON object of spawn arguments forwarded to the adapter
    /// (e.g. `{"args":["--model","sonnet"]}`).
    pub spawn_args: serde_json::Value,
    pub prompt: String,
    pub cron_expr: String,
    pub tz: String,
    pub catchup_mode: String,
    pub max_concurrent_runs: u32,
    pub max_runs_per_day: u32,
    pub max_runtime_s: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_budget_usd_per_day: Option<f64>,
    pub failure_backoff: String,
    pub pause_on_consecutive_failures: u32,
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

// ---------- crons.list ----------

pub enum CronsList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsListParams")]
pub struct CronsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub include_disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "CronsListResult")]
pub struct CronsListResult {
    pub crons: Vec<CronEntry>,
}

impl Method for CronsList {
    const NAME: &'static str = "crons.list";
    type Params = CronsListParams;
    type Result = CronsListResult;
}

// ---------- crons.get ----------

pub enum CronsGet {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsGetParams")]
pub struct CronsGetParams {
    pub cron_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "CronsGetResult")]
pub struct CronsGetResult {
    pub cron: CronEntry,
}

impl Method for CronsGet {
    const NAME: &'static str = "crons.get";
    type Params = CronsGetParams;
    type Result = CronsGetResult;
}

// ---------- crons.upsert ----------

/// Input shape for creating or updating a cron. `id` set ⇒ update;
/// `id` omitted ⇒ daemon mints a fresh UUID v7. All optional knob fields
/// fall back to the architecture §5.4 defaults if omitted.
pub enum CronsUpsert {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "CronsUpsertParams")]
pub struct CronsUpsertParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub project_id: String,
    pub backend: String,
    #[serde(default)]
    pub spawn_args: serde_json::Value,
    pub prompt: String,
    pub cron_expr: String,
    #[serde(default = "default_tz")]
    pub tz: String,
    #[serde(default = "default_catchup_mode")]
    pub catchup_mode: String,
    #[serde(default = "default_max_concurrent_runs")]
    pub max_concurrent_runs: u32,
    #[serde(default = "default_max_runs_per_day")]
    pub max_runs_per_day: u32,
    #[serde(default = "default_max_runtime_s")]
    pub max_runtime_s: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_budget_usd_per_day: Option<f64>,
    #[serde(default = "default_failure_backoff")]
    pub failure_backoff: String,
    #[serde(default = "default_pause_on_failures")]
    pub pause_on_consecutive_failures: u32,
}

fn default_tz() -> String {
    "UTC".to_string()
}
fn default_catchup_mode() -> String {
    "coalesce".to_string()
}
fn default_max_concurrent_runs() -> u32 {
    1
}
fn default_max_runs_per_day() -> u32 {
    24
}
fn default_max_runtime_s() -> u32 {
    1800
}
fn default_failure_backoff() -> String {
    "expo(1m,2,1h)".to_string()
}
fn default_pause_on_failures() -> u32 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "CronsUpsertResult")]
pub struct CronsUpsertResult {
    pub cron: CronEntry,
}

impl Method for CronsUpsert {
    const NAME: &'static str = "crons.upsert";
    type Params = CronsUpsertParams;
    type Result = CronsUpsertResult;
}

// ---------- crons.delete ----------

pub enum CronsDelete {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsDeleteParams")]
pub struct CronsDeleteParams {
    pub cron_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsDeleteResult")]
pub struct CronsDeleteResult {
    pub deleted: bool,
}

impl Method for CronsDelete {
    const NAME: &'static str = "crons.delete";
    type Params = CronsDeleteParams;
    type Result = CronsDeleteResult;
}

// ---------- crons.set_enabled ----------

pub enum CronsSetEnabled {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsSetEnabledParams")]
pub struct CronsSetEnabledParams {
    pub cron_id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "CronsSetEnabledResult")]
pub struct CronsSetEnabledResult {
    pub cron: CronEntry,
}

impl Method for CronsSetEnabled {
    const NAME: &'static str = "crons.set_enabled";
    type Params = CronsSetEnabledParams;
    type Result = CronsSetEnabledResult;
}

// ---------- crons.run_now ----------

/// Fire a cron immediately, bypassing its `cron_expr` schedule but still
/// going through the admission gate (quotas honoured).
pub enum CronsRunNow {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsRunNowParams")]
pub struct CronsRunNowParams {
    pub cron_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "CronsRunNowResult")]
pub struct CronsRunNowResult {
    /// `true` when the admission gate let the fire through and a `runs`
    /// row was created. `false` ⇒ the gate refused; inspect `refused`.
    pub admitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Populated when `admitted = false`; carries the machine-readable
    /// reason tag from [`la_scheduler::AdmissionDecision::error_kind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

impl Method for CronsRunNow {
    const NAME: &'static str = "crons.run_now";
    type Params = CronsRunNowParams;
    type Result = CronsRunNowResult;
}

// ---------- crons.dry_run ----------

/// Pure preview: parse `cron_expr` + `tz`, project the next N fire times.
/// Does NOT touch storage or the heap.
pub enum CronsDryRun {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsDryRunParams")]
pub struct CronsDryRunParams {
    pub cron_expr: String,
    #[serde(default = "default_tz")]
    pub tz: String,
    /// 1..=20; defaults to 5.
    #[serde(default = "default_dry_run_count")]
    pub count: u32,
}

fn default_dry_run_count() -> u32 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronsDryRunResult")]
pub struct CronsDryRunResult {
    /// Projected RFC3339-UTC fire times, earliest first.
    pub fires: Vec<String>,
}

impl Method for CronsDryRun {
    const NAME: &'static str = "crons.dry_run";
    type Params = CronsDryRunParams;
    type Result = CronsDryRunResult;
}

// ---------- runs.list ----------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "RunEntry")]
pub struct RunEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub scheduled_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub coalesced_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_est: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
}

pub enum RunsList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "RunsListParams")]
pub struct RunsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_id: Option<String>,
    /// RFC3339 UTC; daemon converts to SQLite-lexical for the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// 1..=500; default 100.
    #[serde(default = "default_runs_list_limit")]
    pub limit: u32,
}

fn default_runs_list_limit() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "RunsListResult")]
pub struct RunsListResult {
    pub runs: Vec<RunEntry>,
}

impl Method for RunsList {
    const NAME: &'static str = "runs.list";
    type Params = RunsListParams;
    type Result = RunsListResult;
}

// ---------- runs.get ----------

pub enum RunsGet {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "RunsGetParams")]
pub struct RunsGetParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[schemars(rename = "RunsGetResult")]
pub struct RunsGetResult {
    pub run: RunEntry,
}

impl Method for RunsGet {
    const NAME: &'static str = "runs.get";
    type Params = RunsGetParams;
    type Result = RunsGetResult;
}
