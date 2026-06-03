//! Public event stream emitted by the scheduler.
//!
//! Wrapped in a `tokio::sync::mpsc::Receiver<FireEvent>` returned by
//! [`crate::Scheduler::new`]. The la-daemon dispatcher forwards these into
//! the run executor and onto the IPC `cron.fired` notification.
//!
//! We keep the event payload tight on purpose — no `Arc<CronSpec>` or other
//! large values — so the channel buffer stays cheap.

use chrono::{DateTime, Utc};

use crate::heap::CronId;

/// A single scheduled emission. One per fire, including each entry produced
/// by `replay` catch-up; `coalesced_count > 1` only for `coalesce`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireEvent {
    pub cron_id: CronId,
    /// Wall time the cron *should* have fired (becomes `runs.scheduled_at`).
    pub scheduled_at: DateTime<Utc>,
    /// Wall time the scheduler actually emitted this event.
    pub fired_at: DateTime<Utc>,
    /// Number of missed fires merged into this emission (1 for normal
    /// fires and for every entry in a `replay` burst).
    pub coalesced_count: u32,
    /// True if the catch-up resolver hit `MAX_CATCHUP` for the burst this
    /// fire belongs to. The earliest 100 fires were retained (this is one
    /// of them); the rest were dropped and reported separately via
    /// [`SchedulerEvent::CatchupTruncated`] so the UI can surface the loss
    /// (§5.3 / WEK-58). Every fire emitted from an over-cap burst carries
    /// the same `true` value.
    pub catchup_truncated: bool,
}

/// Diagnostic events the scheduler emits when something noteworthy happens
/// outside the normal fire path. Forwarded by the daemon onto the
/// `scheduler.*` metric channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerEvent {
    /// Clock skew exceeded threshold; all entries were re-heaped.
    ClockSkewDetected {
        skew_seconds: i64,
        recomputed_entries: usize,
    },
    /// Catch-up resolver hit `MAX_CATCHUP`; the earliest [`executed`]
    /// fires were retained and the remaining [`dropped`] missed fires were
    /// discarded (§5.3 / WEK-58). Emitted once per over-cap burst, paired
    /// with the matching `catchup_truncated=true` fires.
    ///
    /// [`executed`]: SchedulerEvent::CatchupTruncated::executed
    /// [`dropped`]: SchedulerEvent::CatchupTruncated::dropped
    CatchupTruncated {
        cron_id: String,
        /// Total number of missed fires the resolver was handed.
        missed: usize,
        /// Number of earliest missed fires kept for policy evaluation
        /// (equals `MAX_CATCHUP` by construction).
        executed: usize,
        /// Number of fires dropped (`missed - executed`). May be zero when
        /// `missed == MAX_CATCHUP`.
        dropped: usize,
    },
}
