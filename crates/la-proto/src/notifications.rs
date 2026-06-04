//! Typed payloads for server-pushed notifications.
//!
//! M1.1 covers the full notification surface from architecture §3:
//! - `session.output` — PTY increment (chunked at 64 KiB, see [`crate::chunking`])
//! - `session.state`  — lifecycle transitions
//! - `session.gap`    — backpressure drop notice (paired with `sessions.replay`)
//! - `cron.fired`     — emitted post-M3 once the scheduler is wired up
//! - `daemon.health`  — status-bar pulse
//!
//! The daemon may push some of these only after the client has subscribed
//! via [`crate::methods::EventsSubscribe`] (cron.fired / daemon.health), but
//! `session.output` / `session.state` / `session.gap` are implicitly active
//! for any session the client has attached to.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::methods::SessionState;

/// All notification methods defined by `la-proto`.
///
/// Order matches the structure of architecture §3 (per-session first, then
/// daemon-global). Order is not meaningful at runtime.
pub const NOTIFICATION_NAMES: &[&str] = &[
    SessionOutput::NAME,
    SessionStateNotice::NAME,
    SessionGap::NAME,
    CronFired::NAME,
    DaemonHealth::NAME,
    WorktreeChanged::NAME,
    WorktreeCommitCreated::NAME,
];

/// Trait mirroring [`crate::methods::Method`] but for one-way notifications
/// (no `Result` type).
///
/// Named [`NotificationMethod`] (not `Notification`) to avoid shadowing the
/// [`crate::jsonrpc::Notification`] envelope struct at call sites.
pub trait NotificationMethod {
    const NAME: &'static str;
    type Params: Serialize + for<'de> Deserialize<'de> + JsonSchema;
}

// ---------- session.output ----------

/// PTY output increment for an attached session.
///
/// `seq` is monotonically increasing per session. Chunks larger than
/// [`crate::SESSION_OUTPUT_CHUNK_BYTES`] must be split before being sent;
/// see [`crate::chunking::chunk_session_output`].
pub enum SessionOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionOutputParams")]
pub struct SessionOutputParams {
    pub session_id: String,
    /// Monotonically increasing sequence number, per session. Clients use
    /// gaps to detect dropped frames (architecture §3 关键不变量: 背压).
    pub seq: u64,
    /// Base64-encoded PTY bytes (≤ 64 KiB per notification).
    pub data_base64: String,
}

impl SessionOutputParams {
    /// Construct a single notification payload from raw bytes.
    ///
    /// Per the architecture, a single `session.output` carries at most
    /// [`crate::SESSION_OUTPUT_CHUNK_BYTES`] (64 KiB) decoded bytes. This
    /// constructor does NOT enforce that cap (callers may briefly hold
    /// larger buffers before chunking), but the chunker
    /// [`crate::chunking::chunk_session_output`] is the canonical way to
    /// produce wire-safe values.
    pub fn from_bytes(session_id: impl Into<String>, seq: u64, data: &[u8]) -> Self {
        Self {
            session_id: session_id.into(),
            seq,
            data_base64: B64.encode(data),
        }
    }

    pub fn data_bytes(&self) -> Result<Vec<u8>, base64::DecodeError> {
        B64.decode(self.data_base64.as_bytes())
    }
}

impl NotificationMethod for SessionOutput {
    const NAME: &'static str = "session.output";
    type Params = SessionOutputParams;
}

// ---------- session.state ----------

/// Lifecycle transition for a session (architecture §3:
/// `starting/running/waiting/exited/errored`).
///
/// The type alias `SessionState` is reused from [`crate::methods`] so both
/// surfaces speak exactly one vocabulary.
///
/// Named `SessionStateNotice` (not `SessionState`) on the Rust side to avoid
/// the shared `SessionState` enum collision; on the wire it stays
/// `"session.state"`.
pub enum SessionStateNotice {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionStateParams")]
pub struct SessionStateParams {
    pub session_id: String,
    pub state: SessionState,
    /// Process exit code when `state == Exited`; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Free-form reason populated when `state == Errored` (adapter name +
    /// short description) so the UI can show something more useful than the
    /// raw state string. Other states leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl NotificationMethod for SessionStateNotice {
    const NAME: &'static str = "session.state";
    type Params = SessionStateParams;
}

// ---------- session.gap ----------

/// Notice that the daemon evicted bytes `[from_seq, to_seq]` from a
/// session's ring buffer before the client could consume them
/// (architecture §3 关键不变量: 背压).
///
/// The client should respond by calling [`crate::methods::SessionsReplay`]
/// with `from_seq` if it wants to recover what's still in the buffer; bytes
/// already evicted are gone.
pub enum SessionGap {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionGapParams")]
pub struct SessionGapParams {
    pub session_id: String,
    /// Inclusive lower bound of the missing range.
    pub from_seq: u64,
    /// Inclusive upper bound of the missing range. Equal to `from_seq` when
    /// exactly one chunk was dropped.
    pub to_seq: u64,
    /// Bytes evicted so the client can show a "missed N bytes" hint without
    /// having to round-trip a replay request first.
    pub dropped_bytes: u64,
}

impl NotificationMethod for SessionGap {
    const NAME: &'static str = "session.gap";
    type Params = SessionGapParams;
}

// ---------- cron.fired ----------

/// Cron trigger event for the status bar / run list (architecture §3, §5.5).
/// Sent to clients that subscribed to `EventTopic::CronFired` via
/// [`crate::methods::EventsSubscribe`].
///
/// Defined now so the protocol is stable when M3 wires up the scheduler;
/// daemons that don't yet implement cron simply never push this.
pub enum CronFired {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "CronFiredParams")]
pub struct CronFiredParams {
    pub cron_id: String,
    pub run_id: String,
    /// RFC3339 timestamp of the firing.
    pub fired_at: String,
    /// Status of the run at the moment this notification is emitted (most
    /// commonly `"spawning"` or `"running"`). Use the full `runs.list` for
    /// terminal status — this is the kick-off pulse.
    pub status: String,
}

impl NotificationMethod for CronFired {
    const NAME: &'static str = "cron.fired";
    type Params = CronFiredParams;
}

// ---------- daemon.health ----------

/// Periodic health pulse used by the TUI status bar (architecture §3, §9.3).
/// Pushed roughly every 5 s by the daemon to any client subscribed via
/// `EventTopic::DaemonHealth`.
pub enum DaemonHealth {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "DaemonHealthParams")]
pub struct DaemonHealthParams {
    /// Total number of in-flight + queued cron runs.
    pub queue_depth: u32,
    /// Number of currently running sessions.
    pub running: u32,
    /// Errors observed in the last 5 minutes (any kind, used for the
    /// status-bar dot colour).
    pub errors_last_5m: u32,
    /// Per-backend probe snapshot — additive in M2.6 (`WEK-29`). Clients
    /// that don't understand the field simply ignore it; daemons that
    /// never probe leave it empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<BackendHealth>,
    /// Wire tag of the service supervisor that started this daemon
    /// (`"systemd"` / `"launchd"` / `"windows-task"`). Sourced from
    /// `$LAZYAGENTS_MANAGED_BY` at boot. `None` when the daemon was
    /// started directly (e.g. `lad start` from a shell or
    /// `lad daemonize` from the TUI bootstrap). Additive in WEK-73 /
    /// M4.1; older daemons leave it absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
}

/// Probe state for a single registered adapter, broadcast as part of
/// `daemon.health` so the TUI can render grey-state sidebar entries
/// (architecture §4.3 / `WEK-29`).
///
/// The `status` enum mirrors the variants of [`crate::methods::SessionState`]
/// in spirit but is independent: one is a *session* lifecycle, the other
/// is a *backend* installation state. The two never collapse.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "BackendHealth")]
pub struct BackendHealth {
    /// Stable adapter id, e.g. `"claude"` / `"codex"` / `"opencode"`.
    pub id: String,
    /// Human-readable label suitable for sidebar rendering.
    pub display_name: String,
    /// Probe outcome (`available` / `not_installed` / `unauthenticated` /
    /// `protocol_drift` / `error`).
    pub status: BackendHealthStatus,
    /// Parsed CLI version when the last probe was `Available`; otherwise
    /// `None`. Required by `WEK-29` 验收: "日志包含 backend version".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// One-line reason suitable for UI surface — never sensitive (no
    /// stderr dumps, no command lines). For `Available` ⇒ `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Best-effort docs link for the failure (login page for
    /// `Unauthenticated`, install page for `NotInstalled`, upgrade page
    /// for `ProtocolDrift`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
    /// RFC3339 timestamp of the most recent probe attempt. Empty on the
    /// first pulse before any probe has run.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_probed_at: String,
}

/// Wire representation of a backend's classified probe state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "BackendHealthStatus")]
pub enum BackendHealthStatus {
    /// Installed, runs, authenticated, version parsed.
    Available,
    /// Executable not on `$PATH` (or at the configured override path).
    NotInstalled,
    /// Executable exists but the user has not logged in.
    Unauthenticated,
    /// Backend version returned output the adapter could not parse —
    /// usually means the CLI shipped a breaking change and the adapter
    /// needs an upgrade.
    ProtocolDrift,
    /// Anything else (timeout, permission denied, transport failure).
    Error,
}

impl NotificationMethod for DaemonHealth {
    const NAME: &'static str = "daemon.health";
    type Params = DaemonHealthParams;
}

// ---------- worktree.changed ----------

/// Per-worktree mutation notice (M2.5 / WEK-28). Pushed to every client
/// subscribed via [`crate::methods::EventTopic::WorktreeChanged`] after
/// `worktree.stage` / `worktree.unstage` / `worktree.discard` /
/// `worktree.commit` succeeds, OR after the agent process itself writes
/// to the worktree (the `External` kind — currently delivered on a
/// best-effort polled cadence; a true fs watcher is M3 work).
///
/// Carries only `affected_files` (paths) so the TUI can re-pull
/// `worktree.diff` for the ones it has expanded. No diff bytes ride the
/// notification.
pub enum WorktreeChanged {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeChangedParams")]
pub struct WorktreeChangedParams {
    pub session_id: String,
    /// `"stage" | "unstage" | "discard" | "commit" | "external"`.
    pub kind: String,
    /// Paths relative to the worktree root. Empty when the mutation
    /// scope is the entire worktree (`commit` returns the post-commit
    /// dirty set, which is usually short or empty).
    pub affected_files: Vec<String>,
    /// RFC3339 timestamp of the mutation.
    pub generated_at: String,
}

impl NotificationMethod for WorktreeChanged {
    const NAME: &'static str = "worktree.changed";
    type Params = WorktreeChangedParams;
}

// ---------- worktree.commit_created ----------

/// Notification fired when a `worktree.commit` succeeds. Sibling of
/// [`WorktreeChanged`] — emitted on the same mutation, but on a
/// dedicated topic so a client interested only in commit pulses
/// (toast, "shipped 3 commits" badge) doesn't have to filter `kind`.
pub enum WorktreeCommitCreated {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "WorktreeCommitCreatedParams")]
pub struct WorktreeCommitCreatedParams {
    pub session_id: String,
    /// 40-char hex SHA of the new commit.
    pub commit_sha: String,
    /// First line of the commit message.
    pub summary: String,
    pub files_changed: u32,
    /// RFC3339 timestamp of the commit.
    pub generated_at: String,
}

impl NotificationMethod for WorktreeCommitCreated {
    const NAME: &'static str = "worktree.commit_created";
    type Params = WorktreeCommitCreatedParams;
}
