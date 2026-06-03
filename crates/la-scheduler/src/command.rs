//! Control-plane commands consumed by [`crate::Scheduler::run`].
//!
//! The scheduler loop owns the heap mutex, so external mutation has to go
//! through channels. A single `mpsc` per scheduler instance carries all
//! commands; replies (where applicable) come back via per-command oneshots
//! attached to the command variant.
//!
//! Keeping the command list narrow on purpose: anything more than upsert /
//! delete / shutdown / explicit poke belongs in a higher layer (la-daemon's
//! dispatcher), not here.

use chrono::{DateTime, Utc};
use tokio::sync::oneshot;

use crate::catchup::CatchupMode;
use crate::cron_spec::CronSpec;
use crate::heap::CronId;
use crate::quota::backoff::FailureBackoff;
use crate::Error;

/// One unit of work for the scheduler loop.
#[derive(Debug)]
pub enum Command {
    /// Insert or replace a cron. The scheduler will recompute its next fire
    /// from `now()` (or honour `last_fired_at` so a daemon restart picks up
    /// the catch-up window). Replies with the new version.
    Upsert {
        id: CronId,
        spec: Box<CronSpec>,
        catchup_mode: CatchupMode,
        min_replay_interval: chrono::Duration,
        last_fired_at: Option<DateTime<Utc>>,
        reply: oneshot::Sender<Result<u64, Error>>,
    },
    /// Drop a cron from the heap. Replies with `true` if it existed.
    Delete {
        id: CronId,
        reply: oneshot::Sender<bool>,
    },
    /// Force a re-peek; used by tests and the optional `crons.run_now` path.
    Poke,
    /// Snapshot the current next-fire entries for a status query (status
    /// bar's "next trigger" display, §5.6).
    Snapshot {
        reply: oneshot::Sender<Vec<crate::heap::HeapEntry>>,
    },
    /// Set / clear the per-cron `failure_backoff` mirror after the run
    /// executor settles a terminal run (WEK-52). When the rail is active
    /// (parsed backoff + non-zero counter + recorded last failure) the
    /// scheduler floors `next_fire_at` at
    /// `last_failure_at + delay_for(consecutive_failures)`, so a high-frequency
    /// cron in backoff stops wasting wake-ups firing into an admission gate
    /// that only returns `RefuseDeferBackoff`. Pass `backoff: None` /
    /// `consecutive_failures: 0` to reset the rail (success / paused). Replies
    /// with `true` if the entry existed, `false` if it had been deleted —
    /// the executor treats `false` as "race lost, nothing to do".
    UpdateBackoffState {
        id: CronId,
        backoff: Option<FailureBackoff>,
        last_failure_at: Option<DateTime<Utc>>,
        consecutive_failures: u32,
        reply: oneshot::Sender<bool>,
    },
    /// Stop the loop. The send side is dropped right after; the loop exits
    /// at the next select() iteration.
    Shutdown,
}
