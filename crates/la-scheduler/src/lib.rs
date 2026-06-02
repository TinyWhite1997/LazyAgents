//! # la-scheduler — LazyAgents cron engine
//!
//! Implements ADR-003 (technical architecture §5) of the LazyAgents daemon:
//! a `tokio::time::sleep_until`-driven heap scheduler with IANA-timezone cron
//! expressions, three catch-up policies (skip / coalesce / replay), and a
//! 60-second clock-skew detector that re-anchors all entries when wall-time
//! drifts more than 30 s from monotonic time.
//!
//! ## Module map
//! - [`clock`]: time abstractions; `SystemClock` for prod, `FakeClock` for
//!   `tokio::time::pause`-driven tests.
//! - [`cron_spec`]: cron expression parser (5+6 fields) bound to an IANA tz.
//! - [`catchup`]: pure resolver that turns a missed-fires list into the
//!   emissions for skip / coalesce / replay (§5.3).
//! - [`heap`]: in-memory entry table + lazy-deletion min-heap. The "heap
//!   重排在 upsert/delete 后即时生效" requirement lives here.
//! - [`scheduler`]: the actual `select!` loop and the [`SchedulerHandle`]
//!   callers drive it through.
//! - [`event`], [`command`]: public stream / control surfaces.
//!
//! ## Wire mapping
//! Errors in [`Error`] line up with the `CRON_*` codes in la-proto. The
//! mapping (`InvalidExpr → CRON_INVALID_EXPR (-33302)`, `InvalidTimezone →
//! CRON_INVALID_TZ (-33304)`) is exercised by the la-daemon dispatcher tests;
//! this crate intentionally stays leaf-level and doesn't depend on la-proto.

pub mod catchup;
pub mod clock;
mod command;
pub mod cron_spec;
pub mod error;
pub mod event;
pub mod heap;
mod scheduler;

pub use catchup::{apply_catchup, CatchupMode, CatchupOutcome, ResolvedFire, MAX_CATCHUP};
#[cfg(any(test, feature = "test-util"))]
pub use clock::FakeClock;
pub use clock::{system_clock, Clock, SharedClock, SystemClock};
pub use cron_spec::CronSpec;
pub use error::Error;
pub use event::{FireEvent, SchedulerEvent};
pub use heap::{CronId, Entry, EntryTable, HeapEntry};
pub use scheduler::{Scheduler, SchedulerChannels, SchedulerHandle};
