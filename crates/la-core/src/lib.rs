//! Session manager + global event bus for the LazyAgents daemon (`lad`).
//!
//! Implements **WEK-18 / M1.4** from `report/技术架构设计.md`:
//!
//! - [`SessionManager`] owns the daemon-side lifecycle for every PTY-backed
//!   agent session: `spawn / attach / detach / write / signal` plus
//!   startup-time orphan cleanup. It composes [`la_pty`], [`la_adapter`],
//!   [`la_storage`], and [`la_ipc::OutputHub`] (the multi-attach hub from
//!   WEK-16) into one cohesive façade — `la-core` is the place where
//!   "session = (storage row, PTY child, output hub, parked subs)" is
//!   modelled.
//! - [`EventBus`] is the cross-session broadcast surface that the IPC
//!   dispatcher subscribes to when a client calls `events.subscribe`. It
//!   carries `session.state`, `cron.fired`, and `daemon.health`
//!   notifications — `session.output` stays on the per-session
//!   [`OutputHub`] because each chunk has a private fan-out cost that the
//!   global bus shouldn't replicate.
//!
//! Architecture invariants enforced here:
//!
//! - **§1.2 — `la` 永远不直接持有 PTY**. The PTY child is owned exclusively
//!   by the manager; clients can only reach it via the JSON-RPC surface
//!   (`sessions.write` / `sessions.signal`), so closing a TUI window can
//!   never reap a running agent. The "client disconnect does not kill the
//!   child" unit test in `tests/lifecycle.rs` pins this.
//! - **§3 — 单一写者**. Input ownership is tracked on [`SessionEntry`]; a
//!   second writer is refused with [`CoreError::WriterLocked`]. Read-only
//!   attachers are free.
//! - **§6.2 — ring buffer 2 MiB / session, gap on overflow**. The manager
//!   doesn't re-implement this; it delegates to [`la_ipc::OutputHub`].
//!   `ManagerConfig::hub` exposes the knob for tests.

pub mod error;
pub mod event_bus;
pub mod manager;
pub mod session;
pub mod worktree;

pub use error::CoreError;
pub use event_bus::{BusEvent, EventBus, Topic};
pub use manager::{ManagerConfig, SessionManager, SpawnedSession};
pub use session::{SessionId, SessionStateChange};
pub use worktree::{
    CleanupMode, CommitOutcome, DiffEngine, DiffLine, DiffOutcome, FileEntry, FileKind, FileStatus,
    HookStatus, Hunk, HunkReject, LaunchOutcome, LineOrigin, MutationOutcome, StatusSnapshot,
    TruncationOutcome, WorktreeHandle, WorktreeLocks, WorktreeManager, WorktreePlan,
    MAX_INLINE_DIFF_BYTES, POST_CREATE_HOOK_TIMEOUT,
};

/// Default idle threshold after which a `Running` session is reported as
/// `Waiting` (no PTY output observed for this long).
///
/// Architecture §3 talks about a `waiting` state for "agent is waiting for
/// human input" — there is no in-band signal from the backend that says
/// "I'm idle", so the manager infers it from output silence. The threshold
/// is tunable via [`ManagerConfig::waiting_idle`] for tests.
pub const DEFAULT_WAITING_IDLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Grace period between `spawn()` returning and the first PTY byte before
/// the manager promotes the session from `Starting` to `Running`
/// unconditionally. Without this an interactive backend that opens a
/// prompt and waits for input would never leave `Starting`.
pub const DEFAULT_RUNNING_PROMOTE: std::time::Duration = std::time::Duration::from_millis(250);
