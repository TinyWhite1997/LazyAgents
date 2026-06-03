//! Catch-up policy and the algorithm that turns "list of missed fires" into
//! "list of fires to emit".
//!
//! The architecture (§5.3) calls for three modes and a hard cap of 100:
//! - `skip` — drop everything missed; only fire the next scheduled.
//! - `coalesce` (default) — emit at most one fire for the cluster, tagged
//!   with `coalesced_count = N`.
//! - `replay` — emit all of them in order, throttled by `min_interval`.
//!
//! `max_catchup = 100` is a daemon-wide ceiling. When the backlog crosses
//! `MAX_CATCHUP` the resolver runs the chosen policy over the **earliest**
//! 100 missed fires and reports the rest as dropped (M3 Brief Rev2 §S1 /
//! WEK-58). The caller passes the **real** missed count via
//! [`apply_catchup_with_total`] so the `scheduler.catchup_truncated`
//! metric reflects the true backlog size, not whatever bounded prefix the
//! caller chose to enumerate. The old behaviour — degrading to `skip` and
//! emitting only the most recent fire — is gone.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Hard upper bound on the number of missed-fire timestamps the resolver
/// will execute. When the backlog crosses [`MAX_CATCHUP`] the resolver runs
/// the policy over the earliest [`MAX_CATCHUP`] entries and surfaces the
/// truncation via [`CatchupOutcome::truncated`] (§5.3 / WEK-58).
pub const MAX_CATCHUP: usize = 100;

/// User-selectable catch-up policy. Default is `Coalesce`, matching the
/// architecture doc.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchupMode {
    Skip,
    #[default]
    Coalesce,
    Replay,
}

/// One scheduled emission produced by the catch-up resolver.
///
/// `scheduled_at` is the wall-time the cron *should* have fired (becomes
/// `runs.scheduled_at` in storage). `coalesced_count` is N>1 only for the
/// coalesce policy when ≥2 fires were merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFire {
    pub scheduled_at: DateTime<Utc>,
    pub coalesced_count: u32,
}

/// Truncation summary attached to [`CatchupOutcome`] when the backlog
/// crossed [`MAX_CATCHUP`]. Mirrors the `scheduler.catchup_truncated`
/// metric payload (`cron_id` is added by the caller that has it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupTruncated {
    /// Real number of missed fires the resolver was told about (the
    /// caller's `total_missed`). For the live scheduler this is the value
    /// returned by [`crate::cron_spec::CronSpec::count_missed`]; for the
    /// unit-test entry point it is `missed.len()`.
    pub missed: usize,
    /// Number of earliest missed fires retained for policy evaluation —
    /// always equals [`MAX_CATCHUP`] when this struct is populated, but
    /// stored explicitly so the metric is self-describing.
    pub executed: usize,
    /// Number of fires discarded (`missed - executed`).
    pub dropped: usize,
    /// True when the upstream counter hit its safety cap and `missed` is a
    /// lower bound, not an exact count. The daemon logs this as a separate
    /// warning so a saturating metric never gets mistaken for "exactly
    /// `missed` fires were dropped".
    pub saturated: bool,
}

/// Outcome of running [`apply_catchup_with_total`] on a missed-fire list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupOutcome {
    /// Fires the caller should hand to the run executor, in order.
    pub fires: Vec<ResolvedFire>,
    /// `Some` when the backlog crossed [`MAX_CATCHUP`] and the policy was
    /// run over `earliest_missed[..MAX_CATCHUP]`. The daemon emits a
    /// `scheduler.catchup_truncated` event so the UI can warn that earlier
    /// fires were dropped.
    pub truncated: Option<CatchupTruncated>,
}

/// Resolve a slice of missed wall-time fires into the emissions the daemon
/// should actually execute, given the **real** backlog size.
///
/// - `earliest_missed` is the chronologically earliest prefix of missed
///   fires the caller was willing/able to materialise. The resolver runs
///   the chosen policy over `earliest_missed[..MAX_CATCHUP.min(len)]`.
/// - `total_missed` is the *real* count of missed fires in the catch-up
///   window. Pass `earliest_missed.len()` when there is no separate counter
///   (small backlogs, unit tests).
/// - `total_saturated` is `true` when `total_missed` was bounded by a
///   safety cap upstream; surfaced verbatim on
///   [`CatchupTruncated::saturated`].
///
/// `min_replay_interval` is only honoured when `mode == Replay`.
///
/// `earliest_missed.len() <= total_missed` is an invariant; violating it
/// returns `Err(Error::Invariant)`.
pub fn apply_catchup_with_total(
    earliest_missed: &[DateTime<Utc>],
    total_missed: usize,
    total_saturated: bool,
    mode: CatchupMode,
    min_replay_interval: chrono::Duration,
) -> Result<CatchupOutcome, Error> {
    if earliest_missed.len() > total_missed {
        return Err(Error::Invariant(
            "earliest_missed.len() exceeded total_missed",
        ));
    }
    if total_missed == 0 {
        return Ok(CatchupOutcome {
            fires: Vec::new(),
            truncated: None,
        });
    }

    let (effective, truncated) = if total_missed >= MAX_CATCHUP {
        // The caller might have handed us fewer than MAX_CATCHUP fires
        // (saturating the count_cap but still budgeting a smaller `take`,
        // for example). We honour whatever prefix we got; the truncated
        // metric reflects the real `total_missed` so the dropped count is
        // honest.
        let take = MAX_CATCHUP.min(earliest_missed.len());
        (
            &earliest_missed[..take],
            Some(CatchupTruncated {
                missed: total_missed,
                executed: take,
                dropped: total_missed - take,
                saturated: total_saturated,
            }),
        )
    } else {
        (earliest_missed, None)
    };

    let fires = match mode {
        CatchupMode::Skip => {
            // Drop the backlog entirely. The natural next fire is computed by
            // the scheduler loop after the resolver returns — we don't add it
            // here.
            Vec::new()
        }
        CatchupMode::Coalesce => {
            // One emission carrying the merge count over the kept window.
            vec![ResolvedFire {
                scheduled_at: *effective.last().expect("checked non-empty"),
                coalesced_count: effective.len() as u32,
            }]
        }
        CatchupMode::Replay => {
            let mut out = Vec::with_capacity(effective.len());
            let mut last_kept: Option<DateTime<Utc>> = None;
            for fire in effective {
                let keep = match last_kept {
                    None => true,
                    Some(prev) => {
                        // The architecture spec is "at least min_interval".
                        // chrono::Duration may be zero (no throttle), in
                        // which case every fire is kept.
                        (*fire - prev) >= min_replay_interval
                    }
                };
                if keep {
                    out.push(ResolvedFire {
                        scheduled_at: *fire,
                        coalesced_count: 1,
                    });
                    last_kept = Some(*fire);
                }
            }
            out
        }
    };

    Ok(CatchupOutcome { fires, truncated })
}

/// Convenience entry point for callers that materialise the full missed
/// list themselves (small backlogs, unit tests). The total count and
/// saturation flag are derived from the slice itself, so
/// `apply_catchup(&xs, mode, throttle)` is equivalent to
/// `apply_catchup_with_total(&xs, xs.len(), false, mode, throttle)`.
///
/// Live scheduler code should prefer [`apply_catchup_with_total`] so the
/// `scheduler.catchup_truncated` metric reports the real backlog and not
/// just the bounded prefix the scheduler chose to walk (WEK-58 review
/// blocker).
pub fn apply_catchup(
    missed: &[DateTime<Utc>],
    mode: CatchupMode,
    min_replay_interval: chrono::Duration,
) -> Result<CatchupOutcome, Error> {
    apply_catchup_with_total(missed, missed.len(), false, mode, min_replay_interval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, min, 0).unwrap()
    }

    /// `n` minute-spaced timestamps anchored at 2026-01-01 00:00 UTC.
    fn missed_minutes(n: usize) -> Vec<DateTime<Utc>> {
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        (0..n)
            .map(|m| base + chrono::Duration::minutes(m as i64))
            .collect()
    }

    #[test]
    fn empty_input_yields_empty() {
        let out = apply_catchup(&[], CatchupMode::Coalesce, chrono::Duration::zero()).unwrap();
        assert!(out.fires.is_empty());
        assert!(out.truncated.is_none());
    }

    #[test]
    fn skip_drops_everything() {
        let missed = vec![ts(0), ts(5), ts(10)];
        let out = apply_catchup(&missed, CatchupMode::Skip, chrono::Duration::zero()).unwrap();
        assert!(out.fires.is_empty());
        assert!(out.truncated.is_none());
    }

    #[test]
    fn coalesce_emits_one_with_count() {
        let missed = vec![ts(0), ts(5), ts(10)];
        let out = apply_catchup(&missed, CatchupMode::Coalesce, chrono::Duration::zero()).unwrap();
        assert_eq!(out.fires.len(), 1);
        assert_eq!(out.fires[0].scheduled_at, ts(10));
        assert_eq!(out.fires[0].coalesced_count, 3);
        assert!(out.truncated.is_none());
    }

    #[test]
    fn replay_emits_all_without_throttle() {
        let missed = vec![ts(0), ts(5), ts(10)];
        let out = apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::zero()).unwrap();
        assert_eq!(out.fires.len(), 3);
        for f in &out.fires {
            assert_eq!(f.coalesced_count, 1);
        }
        assert!(out.truncated.is_none());
    }

    #[test]
    fn replay_throttle_drops_too_close() {
        let missed = vec![ts(0), ts(1), ts(5), ts(6), ts(15)];
        let out =
            apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::minutes(5)).unwrap();
        let times: Vec<_> = out.fires.iter().map(|f| f.scheduled_at).collect();
        assert_eq!(times, vec![ts(0), ts(5), ts(15)]);
        assert!(out.truncated.is_none());
    }

    // ---------------------------------------------------------------------
    // §S1 / WEK-58 — over-cap behaviour is "truncate to earliest 100,
    // continue with the chosen policy", *not* degrade-to-skip. The metric
    // must reflect the **real** backlog size, not the bounded prefix.
    // ---------------------------------------------------------------------

    #[test]
    fn over_cap_replay_executes_earliest_100_with_throttle() {
        // Earliest 100 of a 250-fire backlog, 1s apart. min_interval=10s
        // throttle keeps fires at offsets 0, 10, 20, ..., 90 — 10 events.
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let earliest: Vec<_> = (0..MAX_CATCHUP)
            .map(|s| base + chrono::Duration::seconds(s as i64))
            .collect();

        let out = apply_catchup_with_total(
            &earliest,
            250,
            false,
            CatchupMode::Replay,
            chrono::Duration::seconds(10),
        )
        .unwrap();

        let trunc = out
            .truncated
            .as_ref()
            .expect("over-cap must report truncated");
        assert_eq!(trunc.missed, 250, "metric must report the real backlog");
        assert_eq!(trunc.executed, MAX_CATCHUP);
        assert_eq!(trunc.dropped, 150);
        assert!(!trunc.saturated);

        let kept: Vec<_> = out.fires.iter().map(|f| f.scheduled_at).collect();
        let expected: Vec<_> = (0..10)
            .map(|i| base + chrono::Duration::seconds(i * 10))
            .collect();
        assert_eq!(kept, expected);

        // Anti-regression: nothing past the truncation boundary.
        assert!(!kept
            .iter()
            .any(|t| *t >= base + chrono::Duration::seconds(100)));
    }

    #[test]
    fn over_cap_replay_no_throttle_keeps_earliest_100() {
        // 105 missed @ 1 min apart, Replay with zero throttle: expect 100
        // fires (the earliest 100), in order, with dropped=5.
        let missed = missed_minutes(105);
        let out = apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::zero()).unwrap();
        let trunc = out
            .truncated
            .as_ref()
            .expect("over-cap must report truncated");
        assert_eq!(trunc.missed, 105);
        assert_eq!(trunc.executed, MAX_CATCHUP);
        assert_eq!(trunc.dropped, 5);
        assert!(!trunc.saturated);

        assert_eq!(out.fires.len(), MAX_CATCHUP);
        assert_eq!(out.fires.first().unwrap().scheduled_at, missed[0]);
        assert_eq!(
            out.fires.last().unwrap().scheduled_at,
            missed[MAX_CATCHUP - 1]
        );
        for f in &out.fires {
            assert_eq!(f.coalesced_count, 1);
        }
    }

    #[test]
    fn over_cap_coalesce_uses_earliest_100_window() {
        // Coalesce over a 200-fire backlog merges down to one emission
        // anchored at the last *kept* (earliest 100) timestamp with
        // coalesced_count=100, and reports truncated{dropped=100}.
        let missed = missed_minutes(200);
        let out = apply_catchup(&missed, CatchupMode::Coalesce, chrono::Duration::zero()).unwrap();
        let trunc = out
            .truncated
            .as_ref()
            .expect("over-cap must report truncated");
        assert_eq!(trunc.missed, 200);
        assert_eq!(trunc.executed, MAX_CATCHUP);
        assert_eq!(trunc.dropped, 100);
        assert!(!trunc.saturated);

        assert_eq!(out.fires.len(), 1);
        assert_eq!(out.fires[0].coalesced_count, MAX_CATCHUP as u32);
        assert_eq!(out.fires[0].scheduled_at, missed[MAX_CATCHUP - 1]);
    }

    #[test]
    fn over_cap_total_reflects_real_backlog_even_when_prefix_is_smaller() {
        // Scheduler picks `take = MAX_CATCHUP` but the real backlog is
        // 9_999 — the metric must still say `missed=9_999, dropped=9_899`.
        // This is the exact failure mode Code Reviewer flagged.
        let earliest = missed_minutes(MAX_CATCHUP);
        let out = apply_catchup_with_total(
            &earliest,
            9_999,
            false,
            CatchupMode::Coalesce,
            chrono::Duration::zero(),
        )
        .unwrap();
        let trunc = out.truncated.expect("9_999 > MAX_CATCHUP must truncate");
        assert_eq!(
            trunc.missed, 9_999,
            "must report the real backlog, not the prefix"
        );
        assert_eq!(trunc.executed, MAX_CATCHUP);
        assert_eq!(trunc.dropped, 9_899);
        assert!(!trunc.saturated);
    }

    #[test]
    fn over_cap_saturated_flag_propagates() {
        // count_cap was hit upstream — surface that so the daemon log says
        // "at least N", not "exactly N".
        let earliest = missed_minutes(MAX_CATCHUP);
        let out = apply_catchup_with_total(
            &earliest,
            10_000,
            true,
            CatchupMode::Coalesce,
            chrono::Duration::zero(),
        )
        .unwrap();
        let trunc = out.truncated.expect("must truncate");
        assert!(trunc.saturated, "saturated flag must propagate from caller");
        assert_eq!(trunc.missed, 10_000);
        assert_eq!(trunc.dropped, 9_900);
    }

    #[test]
    fn invariant_earliest_longer_than_total_errors() {
        let earliest = missed_minutes(10);
        let err = apply_catchup_with_total(
            &earliest,
            5, // < earliest.len() — illegal
            false,
            CatchupMode::Replay,
            chrono::Duration::zero(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Invariant(_)), "got {err:?}");
    }

    // ---------------------------------------------------------------------
    // Boundary trio: missed=99 / 100 / 101.
    // ---------------------------------------------------------------------

    #[test]
    fn boundary_missed_99_does_not_truncate() {
        let missed = missed_minutes(99);
        let out = apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::zero()).unwrap();
        assert!(
            out.truncated.is_none(),
            "n<MAX_CATCHUP must not report truncated"
        );
        assert_eq!(out.fires.len(), 99);
    }

    #[test]
    fn boundary_missed_100_triggers_truncated_with_dropped_zero() {
        let missed = missed_minutes(100);
        let out = apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::zero()).unwrap();
        let trunc = out
            .truncated
            .as_ref()
            .expect("n==MAX_CATCHUP must report truncated even when nothing was dropped");
        assert_eq!(trunc.missed, 100);
        assert_eq!(trunc.executed, MAX_CATCHUP);
        assert_eq!(trunc.dropped, 0);
        assert!(!trunc.saturated);
        assert_eq!(out.fires.len(), MAX_CATCHUP);
    }

    #[test]
    fn boundary_missed_101_drops_one() {
        let missed = missed_minutes(101);
        let out = apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::zero()).unwrap();
        let trunc = out
            .truncated
            .as_ref()
            .expect("n>MAX_CATCHUP must report truncated");
        assert_eq!(trunc.missed, 101);
        assert_eq!(trunc.executed, MAX_CATCHUP);
        assert_eq!(trunc.dropped, 1);
        assert!(!trunc.saturated);
        assert_eq!(out.fires.len(), MAX_CATCHUP);
        // The dropped entry is the last one (chronologically latest).
        assert_eq!(
            out.fires.last().unwrap().scheduled_at,
            missed[MAX_CATCHUP - 1]
        );
    }
}
