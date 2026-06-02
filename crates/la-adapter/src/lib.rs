//! Agent adapter abstraction for LazyAgents.
//!
//! Implements `§4.1 / ADR-002` from `report/技术架构设计.md`:
//! a backend-agnostic [`AgentAdapter`] trait plus the supporting data
//! types ([`SpawnSpec`], [`ProbeResult`], [`AdapterError`], …) that
//! daemon code uses to drive any concrete CLI backend (`claude`,
//! `codex`, `opencode`, …) without leaking backend-specific details
//! into the rest of the system.
//!
//! Adapters are deliberately *pure*:
//!
//! - they do **not** touch IPC,
//! - they do **not** touch SQLite,
//! - they do **not** own a PTY.
//!
//! They return spec values, consume bytes, and produce events. That
//! makes them easy to unit-test with `cargo test` and a dummy CLI
//! (`tests/bin/mock_cli.rs`).
//!
//! See [`claude::ClaudeAdapter`] for the first concrete implementation
//! shipped in M0.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

pub mod claude;
pub mod codex;
pub mod opencode;

/// Static metadata about an adapter — used for UI listings and the
/// `backends` SQLite snapshot. Field values are stable for the lifetime
/// of a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterDescriptor {
    /// Stable backend id (lower-snake, ASCII): `"claude"`, `"codex"`, …
    pub id: &'static str,
    /// Human-readable label for UI: `"Claude Code"`.
    pub display_name: &'static str,
    /// Default executable name to look up on `$PATH`. Users can
    /// override via config (`adapters.<id>.command`).
    pub default_program: &'static str,
    /// Best-effort link shown in the UI when the backend is detected
    /// but unauthenticated.
    pub docs_url: &'static str,
}

/// Outcome of an adapter probe.
///
/// Per the trait contract, [`AgentAdapter::probe`] never returns
/// `Result::Err` — every failure mode is one of these variants so the
/// UI can render a stable, classified state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// Backend is installed and ready. `version` is the parsed
    /// human-visible version string (e.g. `"2.1.158"`).
    Available { version: String },
    /// Executable was not found on `$PATH` (or at the configured
    /// override path). `hint` is suitable for surface-level UI display.
    NotInstalled { hint: String },
    /// Executable was found and runs, but reports it is not logged in /
    /// not authorised. `docs_url` should be shown to the user.
    Unauthenticated { docs_url: String },
    /// Something else went wrong (timeout, parse error, permission
    /// denied, unexpected exit). `detail` is for logging — UI should
    /// show a generic "probe failed" message.
    Error { detail: String },
}

/// PTY hints from the daemon's session manager to the adapter.
///
/// Adapters typically pass these through verbatim, but may override
/// (e.g. claude requires a specific minimum cols on Windows ConPTY).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyHints {
    pub cols: u16,
    pub rows: u16,
    /// `true` ⇒ disable ICANON so each keystroke is delivered raw.
    /// `false` ⇒ leave line-buffered (good for line-oriented CLIs).
    pub raw_mode: bool,
}

impl Default for PtyHints {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 32,
            raw_mode: false,
        }
    }
}

/// How the child's stdin should be wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StdinMode {
    /// Stdin connected to PTY master — interactive sessions, user can
    /// type / send prompts while the child runs.
    #[default]
    Pty,
    /// Stdin connected to `/dev/null` (or `NUL` on Windows) — used by
    /// cron / unattended runs where there is no human to answer
    /// interactive prompts.
    NullSink,
}

/// Daemon-side request to spawn a new session for a backend.
///
/// The daemon constructs this from the user's UI choices + the project
/// the session belongs to; the adapter translates it into a
/// [`SpawnSpec`] (which the PTY layer then turns into an OS spawn).
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// Optional override for the executable to launch. `None` ⇒ use
    /// [`AdapterDescriptor::default_program`] (resolved on `$PATH`).
    pub program_override: Option<PathBuf>,
    /// Extra positional / flag args appended *after* the
    /// adapter-chosen args.
    pub extra_args: Vec<OsString>,
    /// Working directory for the child — usually the session's git
    /// worktree path (ADR-005).
    pub cwd: PathBuf,
    /// Optional first-turn prompt. When set + [`StdinMode::NullSink`],
    /// adapters that support a non-interactive print mode will pass
    /// the prompt as a flag (e.g. `claude -p "..."`); when set +
    /// [`StdinMode::Pty`], adapters typically *ignore* it in the spec
    /// and the daemon writes it via [`AgentAdapter::encode_user_input`]
    /// after the child is ready.
    pub prompt: Option<String>,
    /// Extra env vars to merge into the spawn (whitelist semantics —
    /// the daemon is responsible for stripping its own env before
    /// passing this in).
    pub env: Vec<(OsString, OsString)>,
    pub pty: PtyHints,
    pub stdin_mode: StdinMode,
}

impl SpawnRequest {
    /// Convenience constructor for tests and one-shot uses.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            program_override: None,
            extra_args: Vec::new(),
            cwd: cwd.into(),
            prompt: None,
            env: Vec::new(),
            pty: PtyHints::default(),
            stdin_mode: StdinMode::Pty,
        }
    }
}

/// Fully resolved spawn description returned by an adapter.
///
/// Pure data; no side effects. The PTY layer (or a test harness) is
/// responsible for actually turning this into a child process. Keeping
/// this type independent of `la-pty` lets `la-adapter` be tested
/// without bringing in the PTY stack.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub pty: PtyHints,
    pub stdin_mode: StdinMode,
}

/// Hints passed to [`AgentAdapter::discover`]. M0 stub — the v1 set is
/// defined in §4.2; for now only the project root is meaningful.
#[derive(Debug, Clone, Default)]
pub struct DiscoverHints {
    pub project_root: Option<PathBuf>,
}

/// A pre-existing session surfaced from a backend's own on-disk store.
/// Mirrors §4.1 of the architecture doc; only the fields that any
/// adapter is expected to populate are included here.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub external_id: String,
    pub project_hint: Option<PathBuf>,
    pub title_hint: Option<String>,
}

/// Parser state threaded through repeated [`AgentAdapter::parse_chunk`]
/// calls. M0 placeholder — adapters that don't implement structured
/// parsing simply ignore it.
#[derive(Debug, Default)]
pub struct ParserState {
    /// Bytes carried over from a chunk that ended mid-token.
    pub partial: Vec<u8>,
}

/// Structured event produced by an adapter that supports parsing the
/// backend's output. M0: only `Passthrough` is produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterEvent {
    /// Raw bytes (no structured interpretation). Always safe to emit;
    /// UI treats it as a transcript chunk.
    Passthrough(Bytes),
}

/// Signals an adapter may request the runtime deliver to the child as
/// part of [`StopSequence`]. Mirrored to `la_pty::Signal` by the
/// daemon; kept separate so `la-adapter` doesn't depend on `la-pty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    Interrupt,
    Terminate,
    Kill,
}

/// One step of a [`StopSequence`].
#[derive(Debug, Clone)]
pub enum StopAction {
    /// Write these bytes to the child's PTY (e.g. `"/exit\n"`).
    SendInput(Bytes),
    /// Send a signal.
    Signal(StopSignal),
    /// Wait for the child to exit on its own, capped at this duration.
    /// If the child exits before the timeout, the runtime should
    /// short-circuit the rest of the sequence.
    AwaitExit(Duration),
}

/// Ordered list of [`StopAction`]s to perform when stopping a session.
///
/// Per §6.4 the default sequence is:
/// 1. send adapter-specific exit input, await 3 s;
/// 2. send `SIGTERM` / `CTRL_BREAK`, await 2 s;
/// 3. send `SIGKILL` / `TerminateProcess`.
#[derive(Debug, Clone)]
pub struct StopSequence(pub Vec<StopAction>);

impl StopSequence {
    /// Convenience: classic 3-step graceful stop without an adapter
    /// input phase (used as the trait-level default).
    pub fn default_signal_only() -> Self {
        Self(vec![
            StopAction::Signal(StopSignal::Terminate),
            StopAction::AwaitExit(Duration::from_secs(2)),
            StopAction::Signal(StopSignal::Kill),
        ])
    }
}

/// Errors returned by adapter methods. Classification matters: the
/// daemon's retry / UI behaviour branches on the variant (see §4.3).
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// Executable not found on `$PATH` or at the override path.
    #[error("not installed: {hint}")]
    NotInstalled { hint: String },
    /// Executable runs but is not logged in / authorised.
    #[error("unauthenticated; see {docs_url}")]
    Unauthenticated { docs_url: String },
    /// OS-level spawn failure (permissions, EACCES, ENOEXEC, …).
    #[error("spawn failed: {0}")]
    SpawnFailed(#[from] std::io::Error),
    /// Caller passed an option the adapter doesn't understand.
    #[error("unsupported option: {name}")]
    UnsupportedOption { name: String },
    /// Backend output format has changed in a way the parser can't
    /// handle. Should escalate to user with an "upgrade" prompt.
    #[error("backend protocol drift: {detail}")]
    ProtocolDrift { detail: String },
    /// Transient failure — caller may retry.
    #[error("transient: {0}")]
    Transient(String),
}

/// The contract every backend adapter implements.
///
/// Methods are split across `sync` and `async` based on whether they
/// perform I/O (probes / discovery do; spec construction and parsing
/// do not). `Send + Sync` is required so adapters live behind an
/// `Arc<dyn AgentAdapter>` in the daemon registry.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Static metadata. Pure function; no I/O.
    fn descriptor(&self) -> AdapterDescriptor;

    /// Probe whether the backend is installed and ready. Must not
    /// return `Result::Err` — every failure case maps to a
    /// [`ProbeResult`] variant.
    async fn probe(&self) -> ProbeResult;

    /// Build the spec needed to spawn a fresh session.
    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, AdapterError>;

    /// Encode a user-typed prompt into the byte stream the backend
    /// expects on its PTY. Adapters differ on line ending / submit key.
    fn encode_user_input(&self, text: &str) -> Bytes;

    /// Adapter-specific stop sequence. Default = signal-only (no
    /// in-band exit command). Adapters that have a built-in `/exit`
    /// should override this.
    fn graceful_stop(&self) -> StopSequence {
        StopSequence::default_signal_only()
    }

    /// Discover pre-existing sessions in the backend's own store.
    /// M0 default = no-op (empty list). Override per §4.2 when adding
    /// real discovery support.
    async fn discover(
        &self,
        _hints: &DiscoverHints,
    ) -> Result<Vec<DiscoveredSession>, AdapterError> {
        Ok(Vec::new())
    }

    /// Parse a chunk of output from the backend's PTY into structured
    /// events. M0 default = `Passthrough` (no structured parsing).
    fn parse_chunk(&self, chunk: &[u8], _st: &mut ParserState) -> Vec<AdapterEvent> {
        if chunk.is_empty() {
            Vec::new()
        } else {
            vec![AdapterEvent::Passthrough(Bytes::copy_from_slice(chunk))]
        }
    }
}
