//! Per-session state owned by [`SessionManager`].
//!
//! `Session` couples the storage row with the live runtime handles:
//! the [`OutputHub`] that fans `session.output` chunks to attached
//! clients, the per-PTY `signal` handle, and the cached state machine
//! transitions used by the bus. Everything that mutates a session lives
//! behind the manager's locks — `Session` itself is a plain data
//! container, no synchronisation.

use std::time::Instant;

use la_ipc::{OutputHub, SubId};
use la_proto::methods::SessionState;
use la_pty::{PtyWriter, Signal};

/// Stable identifier — for now the same UUID v7 string the storage row
/// uses, exposed as its own type so future internal indices don't leak.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// State machine transition the manager publishes on the event bus and
/// persists to SQLite. Mirrors [`SessionState`] one-for-one but adds the
/// optional `exit_code` / `reason` payload the wire notification carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStateChange {
    pub id: SessionId,
    pub state: SessionState,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
}

/// Live handles for an active session. Built by `spawn_session`, stored
/// in [`crate::SessionManager`]'s registry, and torn down on exit /
/// archive / delete.
pub(crate) struct SessionRuntime {
    /// Per-session output fan-out hub. Owns the ring buffer and parked
    /// subscriptions; the manager only calls `publish` / `subscribe` /
    /// `signal_handle`-equivalents on it.
    pub(crate) hub: OutputHub,
    /// Writer half of the PTY. The manager uses this to fulfil
    /// `sessions.write` requests; the child can be signalled separately
    /// via `signal`.
    pub(crate) writer: PtyWriter,
    /// Closure that delivers a [`Signal`] to the live child. Boxed so
    /// `Session` doesn't expose the inner PTY type.
    pub(crate) signaller: SignalFn,
    /// Current lifecycle state, kept in sync with the storage row.
    pub(crate) state: SessionState,
    /// Last exit code observed, or `None` while still alive.
    pub(crate) exit_code: Option<i32>,
    /// `Some(sub)` when a client holds input ownership (per §3 "单一写者"
    /// invariant). Other subscriptions are read-only.
    pub(crate) writer_holder: Option<SubId>,
    /// Wall-clock at which the last PTY byte was observed; used by the
    /// idle-watcher to flip `Running` → `Waiting` after the configured
    /// silence threshold.
    pub(crate) last_output_at: Option<Instant>,
}

/// Boxed signal-delivery function so `Session` doesn't leak `PtyChild`.
pub(crate) type SignalFn =
    Box<dyn Fn(Signal) -> Result<(), la_pty::PtyError> + Send + Sync + 'static>;
