//! Global event bus for non-per-session notifications.
//!
//! Architecture §3 lists three "global" notifications the daemon may push:
//!
//! - `session.state` — lifecycle transitions visible across the sessions list
//! - `cron.fired`    — emitted post-M3 by the scheduler
//! - `daemon.health` — periodic status-bar pulse
//!
//! These are global because every attached client may legitimately want
//! them regardless of which session(s) they have open. We model that as a
//! single [`tokio::sync::broadcast`] channel — subscribers receive every
//! message, and a slow subscriber is the one that pays (with a `Lagged`
//! receive error) rather than blocking publishers. Per-session
//! `session.output` lives on [`la_ipc::OutputHub`] instead, because its
//! per-chunk fan-out cost is high enough that a global broadcast would
//! waste work.

use la_proto::methods::EventTopic;
use la_proto::notifications::{
    CronFiredParams, DaemonHealthParams, SchedulerHealthParams, SessionGapParams,
    SessionStateParams, WorktreeChangedParams, WorktreeCommitCreatedParams,
};
use tokio::sync::broadcast;

/// Capacity of the broadcast channel.
///
/// 256 = plenty of headroom for the 1 Hz `daemon.health` pulse plus
/// occasional bursts of `session.state` transitions. Subscribers that
/// lag past 256 messages will see `RecvError::Lagged` and can drop /
/// resync — these messages are advisory, never load-bearing.
pub const DEFAULT_BUS_CAPACITY: usize = 256;

/// Wire-equivalent topic tag carried on every [`BusEvent`].
///
/// Mirrors [`la_proto::methods::EventTopic`] minus `SessionOutput`, which
/// never travels over this bus (per the module-level doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Topic {
    SessionState,
    SessionGap,
    CronFired,
    DaemonHealth,
    /// Scheduler-loop pulse (M4.4 / WEK-74). Distinct from
    /// [`Self::DaemonHealth`] — adapter probes vs scheduler queue/run
    /// metrics — so subscribers can opt into either independently.
    SchedulerHealth,
    /// Per-worktree mutation pulse (M2.5 / WEK-28).
    WorktreeChanged,
    /// Per-worktree commit pulse (M2.5 / WEK-28).
    WorktreeCommit,
}

impl Topic {
    /// Map to the protocol-level enum so the IPC layer can filter by the
    /// subscription set negotiated in `events.subscribe`.
    pub fn as_proto(self) -> EventTopic {
        match self {
            Topic::SessionState => EventTopic::SessionState,
            Topic::SessionGap => EventTopic::SessionGap,
            Topic::CronFired => EventTopic::CronFired,
            Topic::DaemonHealth => EventTopic::DaemonHealth,
            Topic::SchedulerHealth => EventTopic::SchedulerHealth,
            Topic::WorktreeChanged => EventTopic::WorktreeChanged,
            Topic::WorktreeCommit => EventTopic::WorktreeCommit,
        }
    }

    /// Reverse of [`as_proto`](Self::as_proto). Returns `None` for
    /// `EventTopic::SessionOutput` because that topic does not travel
    /// over the global bus.
    pub fn from_proto(p: EventTopic) -> Option<Self> {
        Some(match p {
            EventTopic::SessionState => Topic::SessionState,
            EventTopic::SessionGap => Topic::SessionGap,
            EventTopic::CronFired => Topic::CronFired,
            EventTopic::DaemonHealth => Topic::DaemonHealth,
            EventTopic::SchedulerHealth => Topic::SchedulerHealth,
            EventTopic::WorktreeChanged => Topic::WorktreeChanged,
            EventTopic::WorktreeCommit => Topic::WorktreeCommit,
            EventTopic::SessionOutput => return None,
        })
    }
}

/// Payload carried on the broadcast channel.
///
/// The enum is intentionally one variant per `Topic` so subscribers can
/// match exhaustively and the compiler catches missing handlers when a new
/// topic lands. The inner types are the same `la_proto` notification
/// params that the IPC dispatcher serialises onto the wire — keeping them
/// here avoids a translation hop.
#[derive(Debug, Clone)]
pub enum BusEvent {
    SessionState(SessionStateParams),
    SessionGap(SessionGapParams),
    CronFired(CronFiredParams),
    DaemonHealth(DaemonHealthParams),
    SchedulerHealth(SchedulerHealthParams),
    WorktreeChanged(WorktreeChangedParams),
    WorktreeCommitCreated(WorktreeCommitCreatedParams),
}

impl BusEvent {
    pub fn topic(&self) -> Topic {
        match self {
            BusEvent::SessionState(_) => Topic::SessionState,
            BusEvent::SessionGap(_) => Topic::SessionGap,
            BusEvent::CronFired(_) => Topic::CronFired,
            BusEvent::DaemonHealth(_) => Topic::DaemonHealth,
            BusEvent::SchedulerHealth(_) => Topic::SchedulerHealth,
            BusEvent::WorktreeChanged(_) => Topic::WorktreeChanged,
            BusEvent::WorktreeCommitCreated(_) => Topic::WorktreeCommit,
        }
    }
}

/// Daemon-side broadcast hub. Cheap to clone — internally a
/// [`tokio::sync::broadcast::Sender`]. Publishers and subscribers can hold
/// independent clones.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<BusEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_BUS_CAPACITY)
    }
}

impl EventBus {
    pub fn with_capacity(cap: usize) -> Self {
        let (tx, _rx) = broadcast::channel(cap.max(1));
        Self { tx }
    }

    /// Subscribe to every event. The dispatcher filters by the topic set
    /// negotiated in `events.subscribe` before serializing onto the wire.
    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    /// Publish an event. Returns the number of *active* receivers the
    /// message was sent to (0 = nobody listening — still success because
    /// the broadcast channel intentionally has no required-subscriber
    /// guarantee).
    pub fn publish(&self, event: BusEvent) -> usize {
        // `send` returns Err only when there are no subscribers; treat
        // that as success-with-zero-recipients rather than propagating,
        // because the bus is fire-and-forget by design.
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscriber count, for tests and `daemon.health` reporting.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}
