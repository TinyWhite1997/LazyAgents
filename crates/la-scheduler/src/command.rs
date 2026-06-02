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
    /// Stop the loop. The send side is dropped right after; the loop exits
    /// at the next select() iteration.
    Shutdown,
}
