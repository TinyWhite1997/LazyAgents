//! Typed parameters and results for the M0.2 minimum method set.
//!
//! Per the issue (WEK-12), M0.2 must implement five wire methods:
//! `initialize`, `sessions.create`, `sessions.attach`, `sessions.write`, and
//! the server notification `session.output`. Other methods listed in
//! architecture §3 are out of scope until later milestones; only the five
//! shipped here are guaranteed by [`METHOD_NAMES`].
//!
//! Each method type implements [`Method`], which centralizes the method name
//! string so dispatch / schema export stays in lock-step with the type.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// All RPC methods defined by `la-proto` at M0.2 (in the order they are
/// expected to be called by a fresh client).
///
/// Used by the dispatcher and by `gen_schema` to enumerate types.
pub const METHOD_NAMES: &[&str] = &[
    Initialize::NAME,
    SessionsCreate::NAME,
    SessionsAttach::NAME,
    SessionsWrite::NAME,
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
}

impl Method for Initialize {
    const NAME: &'static str = "initialize";
    type Params = InitializeParams;
    type Result = InitializeResult;
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
    /// Replay window: the daemon should resend at most this many bytes from
    /// its ring buffer before live streaming. `None` ⇒ daemon default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_bytes: Option<u64>,
    /// Request input ownership. Only one writer per session at a time
    /// (architecture §3 关键不变量).
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
}

impl Method for SessionsAttach {
    const NAME: &'static str = "sessions.attach";
    type Params = SessionsAttachParams;
    type Result = SessionsAttachResult;
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
