//! Catch-up policy and the algorithm that turns "list of missed fires" into
//! "list of fires to emit".
//!
//! The architecture (§5.3) calls for three modes and a hard cap of 100:
//! - `skip` — drop everything missed; only fire the next scheduled.
//! - `coalesce` (default) — emit at most one fire for the cluster, tagged
//!   with `coalesced_count = N`.
//! - `replay` — emit all of them in order, throttled by `min_interval`.
//!
//! `max_catchup = 100` is a daemon-wide ceiling. When `missed.len() >=
//! MAX_CATCHUP`, the resolver truncates the missed list to the **earliest**
//! 100 entries (M3 Brief Rev2 §S1 / WEK-58): the policy then runs over those
//! 100 as if they were the only missed fires, and the caller is told via
//! [`CatchupOutcome::truncated`] how many were dropped. The old behaviour —
//! degrading to `skip` and emitting only the most recent fire — discarded
//! useful catch-up coverage and is no longer used.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Hard upper bound on the number of missed-fire timestamps the resolver
/// will *consider*. When `missed.len() >= MAX_CATCHUP`, [`apply_catchup`]
/// truncates the input to the earliest [`MAX_CATCHUP`] entries and reports
/// the truncation via [`CatchupOutcome::truncated`] (§5.3 / WEK-58).
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

/// Truncation summary attached to [`CatchupOutcome`] when the missed-fire
/// input crossed [`MAX_CATCHUP`]. Mirrors the `scheduler.catchup_truncated`
/// metric payload (`cron_id` is added by the caller that has it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupTruncated {
    /// Total number of missed fires the resolver was handed.
    pub missed: usize,
    /// Number of earliest missed fires retained for policy evaluation —
    /// always equals [`MAX_CATCHUP`] when this struct is populated, but
    /// stored explicitly so the metric is self-describing.
    pub executed: usize,
    /// Number of fires discarded (`missed - executed`). May be zero when
    /// `missed.len() == MAX_CATCHUP`.
    pub dropped: usize,
}

/// Outcome of running [`apply_catchup`] on a missed-fire list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupOutcome {
    /// Fires the caller should hand to the run executor, in order.
    pub fires: Vec<ResolvedFire>,
    /// `Some` when [`MAX_CATCHUP`] was reached and the input was sliced to
    /// the earliest 100 entries before policy evaluation. The daemon emits a
    /// `scheduler.catchup_truncated` event so the UI can warn that earlier
    /// fires were dropped.
    pub truncated: Option<CatchupTruncated>,
}

/// Resolve a slice of missed wall-time fires into the emissions the daemon
/// should actually execute.
///
/// `missed` must be in chronological order; an empty slice yields no fires.
/// `min_replay_interval` is only honoured when `mode == Replay` — it filters
/// out fires that would land less than `min_replay_interval` after the
/// previously kept fire, modelling the "replay throttle" from §5.3.
///
/// When `missed.len() >= MAX_CATCHUP`, the input is truncated to the
/// **earliest** [`MAX_CATCHUP`] entries before the policy runs — the older
/// half of the backlog is preserved so the cron can pick the run history up
/// from where it stopped, instead of the previous behaviour of jumping to
/// the most recent fire and discarding everything before it (WEK-58).
pub fn apply_catchup(
    missed: &[DateTime<Utc>],
    mode: CatchupMode,
    min_replay_interval: chrono::Duration,
) -> Result<CatchupOutcome, Error> {
    if missed.is_empty() {
        return Ok(CatchupOutcome {
            fires: Vec::new(),
            truncated: None,
        });
    }

    let n = missed.len();
    let (effective, truncated) = if n >= MAX_CATCHUP {
        (
            &missed[..MAX_CATCHUP],
            Some(CatchupTruncated {
                missed: n,
                executed: MAX_CATCHUP,
                dropped: n - MAX_CATCHUP,
            }),
        )
    } else {
        (missed, None)
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
    // continue with the chosen policy", *not* degrade-to-skip.
    // ---------------------------------------------------------------------

    #[test]
    fn over_cap_replay_executes_earliest_100_with_throttle() {
        // 250 missed fires, 1s apart. min_interval=10s throttle means the
        // post-truncate Replay should emit fires at offsets 0, 10, 20, ...,
        // 90 (10 fires from the first 100 entries).
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let missed: Vec<_> = (0..250usize)
            .map(|s| base + chrono::Duration::seconds(s as i64))
            .collect();

        let out =
            apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::seconds(10)).unwrap();

        let trunc = out
            .truncated
            .as_ref()
            .expect("over-cap must report truncated");
        assert_eq!(trunc.missed, 250);
        assert_eq!(trunc.executed, MAX_CATCHUP);
        assert_eq!(trunc.dropped, 150);

        // Throttle keeps 1 fire per 10s window across the earliest 100s of
        // the backlog: t=0,10,20,...,90 inclusive = 10 fires. Notably,
        // anything past t=99s (the truncation boundary) MUST NOT appear.
        let kept: Vec<_> = out.fires.iter().map(|f| f.scheduled_at).collect();
        let expected: Vec<_> = (0..10)
            .map(|i| base + chrono::Duration::seconds(i * 10))
            .collect();
        assert_eq!(kept, expected);

        // Anti-regression: the boundary fire at t=99s must be kept by the
        // truncate step but filtered by the 10s throttle (last kept was
        // t=90s). The post-truncate t=100s entry must have been dropped.
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

        assert_eq!(out.fires.len(), 1);
        assert_eq!(out.fires[0].coalesced_count, MAX_CATCHUP as u32);
        assert_eq!(out.fires[0].scheduled_at, missed[MAX_CATCHUP - 1]);
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
        assert_eq!(out.fires.len(), MAX_CATCHUP);
        // The dropped entry is the last one (chronologically latest).
        assert_eq!(
            out.fires.last().unwrap().scheduled_at,
            missed[MAX_CATCHUP - 1]
        );
    }
}
