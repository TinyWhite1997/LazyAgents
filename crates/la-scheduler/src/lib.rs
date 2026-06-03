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
//!
//! ## §5.4 failure_backoff — scheduler-side deferral (WEK-52)
//!
//! `crates/la-scheduler/src/quota` owns the admission *evaluation* (the gate
//! the daemon's executor calls before spawning a session); the scheduler
//! *loop* owns the heap-side deferral so a cron in backoff stops waking the
//! loop on every cron tick. Wiring:
//!
//! 1. After each terminal run the executor calls
//!    [`SchedulerHandle::update_backoff_state`] (failure path) or
//!    [`SchedulerHandle::clear_backoff_state`] (success / non-failure path)
//!    with the new `consecutive_failures` counter and the wall-clock
//!    timestamp of the most recent terminal failure.
//! 2. The scheduler mirrors the three fields onto the entry and re-anchors
//!    `next_fire_at = max(spec.next_after(now), last_failure_at + delay_for(n))`
//!    via [`next_eligible_fire`]. Every other place that would have called
//!    `spec.next_after(now)` (install, post-fire reschedule, skew recompute)
//!    routes through the same helper so the floor is uniform.
//! 3. The admission gate ([`evaluate_admission`]) keeps its
//!    [`AdmissionDecision::RefuseDeferBackoff`] branch as a safety net for
//!    callers that bypass the scheduler (e.g. `crons.run_now`), but in steady
//!    state the scheduler should never push a fire that the gate would
//!    refuse for backoff reasons.
//!
//! ### Install / daemon-restart seeding (REQUIRED)
//!
//! The scheduler's backoff mirror is **in-memory only** — it does not survive
//! a daemon restart, and a fresh [`SchedulerHandle::upsert`] starts with a
//! zero [`BackoffState`]. The authoritative copy lives in SQLite on the
//! `crons` row (`consecutive_failures`, parsed `failure_backoff`) plus the
//! `runs` row that holds the most recent terminal failure's `finished_at`.
//!
//! Therefore the daemon **MUST**, immediately after every
//! `SchedulerHandle::upsert` whose row has `consecutive_failures > 0`, follow
//! with a [`SchedulerHandle::update_backoff_state`] call seeded from SQLite.
//! Without that follow-up, every daemon restart silently disables the heap
//! floor until the next terminal failure observation re-arms it: the
//! admission gate still refuses the fires (it re-reads SQLite) but the
//! scheduler reverts to waking on every cron tick — exactly the wake-up
//! noise this layer exists to suppress.
//!
//! This contract is enforced by the daemon's run-executor brief (M3.5);
//! la-scheduler exposes the API but cannot police the caller.

pub mod catchup;
pub mod clock;
mod command;
pub mod cron_spec;
pub mod error;
pub mod event;
pub mod heap;
pub mod quota;
mod scheduler;

pub use catchup::{
    apply_catchup, CatchupMode, CatchupOutcome, CatchupTruncated, ResolvedFire, MAX_CATCHUP,
};
#[cfg(any(test, feature = "test-util"))]
pub use clock::FakeClock;
pub use clock::{system_clock, Clock, SharedClock, SystemClock};
pub use cron_spec::CronSpec;
pub use error::Error;
pub use event::{FireEvent, SchedulerEvent};
pub use heap::{next_eligible_fire, BackoffState, CronId, Entry, EntryTable, HeapEntry};
pub use quota::{
    evaluate_admission, max_runtime, should_auto_pause, AdmissionDecision, CronQuota,
    FailureBackoff, GlobalQuota, QuotaSnapshot,
};
pub use scheduler::{Scheduler, SchedulerChannels, SchedulerHandle};
