//! Time abstractions for [`crate::Scheduler`].
//!
//! Production code uses [`SystemClock`], which wires `chrono::Utc::now()` to a
//! monotonic [`tokio::time::Instant`] anchor. Tests build a [`FakeClock`] on
//! top of `tokio::time::pause` so simulated wall and monotonic time advance
//! together — that's what lets us run a "seven-day timeline" inside a unit
//! test in milliseconds.
//!
//! The split matters because the scheduler needs **both** views: it sleeps on
//! the monotonic clock (so suspend/resume doesn't cause silent gaps and DST
//! shifts don't trip us) but reasons about cron expressions in IANA-zoned
//! wall time. Mixing the two through a single trait is intentional — the
//! clock-skew detector compares the two deltas and triggers a full re-heap
//! when they disagree by more than 30 s (§5.2).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::time::Instant;

/// Source of truth for "what time is it" inside the scheduler.
///
/// Implementations MUST keep `wall_now()` and `mono_now()` advancing in
/// lock-step under normal operation; the only legitimate divergence is a real
/// clock skew (NTP step, laptop suspend/resume) — exactly what
/// [`crate::Scheduler`]'s 60 s skew detector is designed to catch.
pub trait Clock: Send + Sync + 'static {
    /// IANA wall time, used to evaluate cron expressions and emit
    /// `scheduled_at` timestamps.
    fn wall_now(&self) -> DateTime<Utc>;
    /// Monotonic Tokio time, used to drive `sleep_until` deadlines. Distinct
    /// from `wall_now` so suspend/resume can't silently push deadlines.
    fn mono_now(&self) -> Instant;
}

/// Production clock: real wall time + real Tokio `Instant::now()`.
#[derive(Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn wall_now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn mono_now(&self) -> Instant {
        Instant::now()
    }
}

/// Convert a wall-clock deadline into a Tokio monotonic deadline using `clock`
/// as the conversion anchor.
///
/// If `deadline` is already in the past relative to `wall_now()` we clamp to
/// `mono_now()` so the caller's `sleep_until` returns immediately — that's how
/// catch-up after a clock jump turns into "fire as soon as possible".
pub fn wall_to_instant(clock: &dyn Clock, deadline: DateTime<Utc>) -> Instant {
    let now_wall = clock.wall_now();
    let now_mono = clock.mono_now();
    if deadline <= now_wall {
        return now_mono;
    }
    let delta = deadline - now_wall;
    match delta.to_std() {
        Ok(d) => now_mono + d,
        // delta > i64::MAX seconds: clamp to a far-future deadline (~100 years
        // is plenty — the scheduler's clock-skew tick will revisit long before
        // anyone fires this).
        Err(_) => now_mono + std::time::Duration::from_secs(60 * 60 * 24 * 365 * 100),
    }
}

/// Convenience type alias for clocks passed into [`crate::Scheduler::new`].
pub type SharedClock = Arc<dyn Clock>;

/// Trivial constructor for production callers.
pub fn system_clock() -> SharedClock {
    Arc::new(SystemClock)
}

// ---------------------------------------------------------------------------
// Test clock
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-util"))]
pub use fake::FakeClock;

#[cfg(any(test, feature = "test-util"))]
mod fake {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Mutex;

    /// Test clock that pins wall time to Tokio's mocked monotonic clock.
    ///
    /// Behaviour:
    /// - `wall_now()` = `wall_epoch + (mono_now() - mono_epoch) + wall_offset`
    /// - `mono_now()` = `tokio::time::Instant::now()`
    ///
    /// In a `#[tokio::test(start_paused = true)]` test, advancing Tokio time
    /// via `tokio::time::advance` advances both `mono_now` and `wall_now` by
    /// the same amount, modelling a healthy clock. Calling
    /// [`FakeClock::inject_wall_skew`] adds a one-shot offset to `wall_now`
    /// *without* moving `mono_now`, which is exactly the "NTP stepped the
    /// clock" / "laptop woke from suspend" scenario the skew detector exists
    /// to handle.
    pub struct FakeClock {
        wall_epoch: DateTime<Utc>,
        mono_epoch: Instant,
        wall_offset: Mutex<chrono::Duration>,
    }

    impl FakeClock {
        /// Anchor the fake wall clock at `wall_epoch` and the fake monotonic
        /// clock at "Tokio's current Instant". Call from inside a
        /// `start_paused` Tokio test.
        pub fn new(wall_epoch: DateTime<Utc>) -> Self {
            Self {
                wall_epoch,
                mono_epoch: Instant::now(),
                wall_offset: Mutex::new(chrono::Duration::zero()),
            }
        }

        /// Anchor at a specific `y-m-d h:m:s` UTC.
        pub fn at_utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Arc<Self> {
            let wall = Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap();
            Arc::new(Self::new(wall))
        }

        /// Add `delta` to `wall_now()` without touching `mono_now()`.
        ///
        /// The shift is permanent (additive); call repeatedly to model
        /// successive skews. Drives the scheduler's 60-s skew detector.
        pub fn inject_wall_skew(&self, delta: chrono::Duration) {
            let mut guard = self.wall_offset.lock().unwrap();
            *guard += delta;
        }
    }

    impl Clock for FakeClock {
        fn wall_now(&self) -> DateTime<Utc> {
            let elapsed_std = Instant::now().saturating_duration_since(self.mono_epoch);
            let elapsed = chrono::Duration::from_std(elapsed_std).unwrap_or(chrono::Duration::MAX);
            let offset = *self.wall_offset.lock().unwrap();
            self.wall_epoch + elapsed + offset
        }
        fn mono_now(&self) -> Instant {
            Instant::now()
        }
    }
}
