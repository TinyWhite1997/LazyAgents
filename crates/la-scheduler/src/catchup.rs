//! Catch-up policy and the algorithm that turns "list of missed fires" into
//! "list of fires to emit".
//!
//! The architecture (§5.3) calls for three modes and a hard cap of 100:
//! - `skip` — drop everything missed; only fire the next scheduled.
//! - `coalesce` (default) — emit at most one fire for the cluster, tagged
//!   with `coalesced_count = N`.
//! - `replay` — emit all of them in order, throttled by `min_interval`.
//!
//! `max_catchup = 100` is a daemon-wide ceiling: any policy that would emit
//! more than 100 entries is forcibly downgraded to `skip` and a warning event
//! is recorded. This is the "system was off for a year" safety belt.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Hard upper bound on catch-up emissions per cron, per recovery pass.
/// Above this, [`apply_catchup`] forces `skip` and stamps a warning so the
/// caller can surface it to the user (§5.3).
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

/// Outcome of running [`apply_catchup`] on a missed-fire list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupOutcome {
    /// Fires the caller should hand to the run executor, in order.
    pub fires: Vec<ResolvedFire>,
    /// True if [`MAX_CATCHUP`] was hit and we degraded to `skip`. The daemon
    /// is expected to write a warning event so the UI can surface it.
    pub degraded_to_skip: bool,
}

/// Resolve a slice of missed wall-time fires into the emissions the daemon
/// should actually execute.
///
/// `missed` must be in chronological order; an empty slice yields no fires.
/// `min_replay_interval` is only honoured when `mode == Replay` — it filters
/// out fires that would land less than `min_replay_interval` after the
/// previously kept fire, modelling the "replay throttle" from §5.3.
pub fn apply_catchup(
    missed: &[DateTime<Utc>],
    mode: CatchupMode,
    min_replay_interval: chrono::Duration,
) -> Result<CatchupOutcome, Error> {
    if missed.is_empty() {
        return Ok(CatchupOutcome {
            fires: Vec::new(),
            degraded_to_skip: false,
        });
    }

    let n = missed.len();
    if n > MAX_CATCHUP {
        // §5.3: above the cap, force skip + warn. We still emit ONE fire (the
        // most recent) so the cron resumes immediately rather than waiting
        // for its next natural tick — that matches "skip the backlog, but
        // don't pretend the cron didn't fire at all".
        return Ok(CatchupOutcome {
            fires: vec![ResolvedFire {
                scheduled_at: *missed.last().expect("checked non-empty"),
                coalesced_count: 1,
            }],
            degraded_to_skip: true,
        });
    }

    let fires = match mode {
        CatchupMode::Skip => {
            // Drop the backlog entirely. The natural next fire is computed by
            // the scheduler loop after the resolver returns — we don't add it
            // here.
            Vec::new()
        }
        CatchupMode::Coalesce => {
            // One emission carrying the merge count.
            vec![ResolvedFire {
                scheduled_at: *missed.last().expect("checked non-empty"),
                coalesced_count: n as u32,
            }]
        }
        CatchupMode::Replay => {
            let mut out = Vec::with_capacity(n);
            let mut last_kept: Option<DateTime<Utc>> = None;
            for fire in missed {
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

    Ok(CatchupOutcome {
        fires,
        degraded_to_skip: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, min, 0).unwrap()
    }

    #[test]
    fn empty_input_yields_empty() {
        let out = apply_catchup(&[], CatchupMode::Coalesce, chrono::Duration::zero()).unwrap();
        assert!(out.fires.is_empty());
        assert!(!out.degraded_to_skip);
    }

    #[test]
    fn skip_drops_everything() {
        let missed = vec![ts(0), ts(5), ts(10)];
        let out = apply_catchup(&missed, CatchupMode::Skip, chrono::Duration::zero()).unwrap();
        assert!(out.fires.is_empty());
    }

    #[test]
    fn coalesce_emits_one_with_count() {
        let missed = vec![ts(0), ts(5), ts(10)];
        let out = apply_catchup(&missed, CatchupMode::Coalesce, chrono::Duration::zero()).unwrap();
        assert_eq!(out.fires.len(), 1);
        assert_eq!(out.fires[0].scheduled_at, ts(10));
        assert_eq!(out.fires[0].coalesced_count, 3);
    }

    #[test]
    fn replay_emits_all_without_throttle() {
        let missed = vec![ts(0), ts(5), ts(10)];
        let out = apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::zero()).unwrap();
        assert_eq!(out.fires.len(), 3);
        for f in &out.fires {
            assert_eq!(f.coalesced_count, 1);
        }
    }

    #[test]
    fn replay_throttle_drops_too_close() {
        let missed = vec![ts(0), ts(1), ts(5), ts(6), ts(15)];
        let out =
            apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::minutes(5)).unwrap();
        let times: Vec<_> = out.fires.iter().map(|f| f.scheduled_at).collect();
        assert_eq!(times, vec![ts(0), ts(5), ts(15)]);
    }

    #[test]
    fn over_cap_degrades_to_skip_keeping_last() {
        let missed: Vec<_> = (0..(MAX_CATCHUP as u32 + 5))
            .map(|m| {
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                    + chrono::Duration::minutes(m as i64)
            })
            .collect();
        let out = apply_catchup(&missed, CatchupMode::Replay, chrono::Duration::zero()).unwrap();
        assert!(out.degraded_to_skip);
        assert_eq!(out.fires.len(), 1);
        assert_eq!(out.fires[0].scheduled_at, *missed.last().unwrap());
    }
}
