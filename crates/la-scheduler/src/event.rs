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
    /// True if the catch-up resolver hit `MAX_CATCHUP` and forced this fire
    /// from the "skip the backlog, run once" fallback. UI should warn the
    /// user that earlier fires were dropped.
    pub catchup_degraded: bool,
}

/// Diagnostic events the scheduler emits when something noteworthy happens
/// outside the normal fire path. Currently only one variant, but the enum is
/// public so the daemon can add structured handling later (UI badges, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerEvent {
    /// Clock skew exceeded threshold; all entries were re-heaped.
    ClockSkewDetected {
        skew_seconds: i64,
        recomputed_entries: usize,
    },
}
