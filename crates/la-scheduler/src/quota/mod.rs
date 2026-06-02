//! Per-cron + global admission control for the cron scheduler.
//!
//! Implements WEK-33 / M3.2 of `report/技术架构设计.md` §5.4 ("并发与花费上限"):
//! every [`FireEvent`](crate::FireEvent) the daemon receives from the
//! scheduler must pass through this gate before the run executor spawns a
//! session. The gate refuses the fire (and records an audit row) when any
//! one of the per-cron quotas — `max_concurrent_runs`, `max_runs_per_day`,
//! `cost_budget_usd_per_day`, `pause_on_consecutive_failures` — or either
//! of the global knobs — `global_max_concurrent_runs`, `cpu_load_throttle`
//! — would be violated by spawning.
//!
//! This module is intentionally pure: it takes a [`CronQuota`] config plus a
//! [`QuotaSnapshot`] of current counters and returns an
//! [`AdmissionDecision`]. The downstream wiring (loading the snapshot from
//! the SQLite repos, calling `RunsRepo::create_rejected`, calling
//! `CronsRepo::pause_for_failures` after the threshold trips) lives in
//! la-daemon — la-scheduler stays leaf-level (no la-storage / la-proto
//! dependency), exactly the boundary §5.4 implies and the la-scheduler
//! crate-level doc affirms.
//!
//! ## What this module is NOT
//!
//! It is NOT the run executor. It does not own the daemon-wide "running run
//! count" Mutex. It does not spawn sessions. It does not write to SQLite.
//! It does not decide *how* to handle a refusal (drop, postpone, audit-only)
//! — it returns the decision; the caller acts.
//!
//! ## Sliding-window semantics
//!
//! `max_runs_per_day` is a **24h rolling window**, matching §5.4
//! "24h 滚动窗口硬上限". The caller passes today's window start
//! (`now - 24h`) into [`QuotaSnapshot::window_runs_today`] and
//! [`QuotaSnapshot::window_cost_today`]; the gate compares against that
//! pre-summed count rather than re-aggregating. A coalesced fire (one
//! [`FireEvent`] carrying `coalesced_count = N`) counts as a single attempt
//! against the per-day cap because the user-observable "this cron tried to
//! fire just now" is one attempt — the suppressed N-1 missed fires don't
//! get charged.

pub mod backoff;
pub mod loadavg;

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use backoff::FailureBackoff;

/// Per-cron quota configuration as stored on the `crons` row (architecture
/// §5.4 + storage migration 0004). All integer fields are `u32` because
/// each is constrained `>= 1` (or `>= 0` for the counter) by the schema
/// CHECKs; modelling them as signed `i64` only invites later confusion.
///
/// `cost_budget_usd_per_day = None` means "unbounded" — the gate skips the
/// cost dimension entirely. `pause_on_consecutive_failures = 0` is a
/// schema-rejected value; we treat it as "never auto-pause" defensively if
/// it ever appears.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CronQuota {
    pub max_concurrent_runs: u32,
    pub max_runs_per_day: u32,
    pub max_runtime_s: u32,
    pub cost_budget_usd_per_day: Option<f64>,
    pub pause_on_consecutive_failures: u32,
    /// Current value of the persisted counter. Bumped on terminal failure
    /// by the run executor; reset to zero on a `completed` run.
    pub consecutive_failures: u32,
    /// Parsed `failure_backoff` DSL (architecture §5.4 "连续失败时延后下一次
    /// 触发"). The gate uses this together with
    /// [`QuotaSnapshot::last_terminal_failure_at`] to defer the fire until
    /// `last_terminal_failure_at + backoff.delay_for(consecutive_failures)`.
    /// `None` means "no backoff configured" (gate is inactive on this rail);
    /// callers should fall back to the DDL default if parsing the stored
    /// string failed rather than disabling the rail silently.
    pub failure_backoff: Option<FailureBackoff>,
    /// Mirrors `crons.enabled`. A `false` value short-circuits to
    /// [`AdmissionDecision::RefusePaused`] regardless of the other fields,
    /// because an auto-pause writer (or a user toggle) has already decided
    /// this cron should not fire.
    pub enabled: bool,
}

impl Default for CronQuota {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 1,
            max_runs_per_day: 24,
            max_runtime_s: 1800,
            cost_budget_usd_per_day: None,
            pause_on_consecutive_failures: 5,
            consecutive_failures: 0,
            failure_backoff: Some(FailureBackoff::default()),
            enabled: true,
        }
    }
}

/// Daemon-wide scheduler knobs from `[scheduler]` (§5.4 + §11.1). Loaded
/// once at daemon startup; not per-cron.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GlobalQuota {
    /// Hard ceiling on concurrent in-flight cron-spawned runs across the
    /// whole daemon (§5.4 default 8). `0` disables the gate.
    pub global_max_concurrent_runs: u32,
    /// 1-minute loadavg threshold; on Unix, when sampled load exceeds this,
    /// admission is deferred. `None` disables the gate; on platforms
    /// without a meaningful loadavg (Windows v1) this is silently
    /// inactive even when configured.
    pub cpu_load_throttle: Option<f64>,
}

impl Default for GlobalQuota {
    fn default() -> Self {
        // Architecture §5.4 / §11.1 defaults.
        Self {
            global_max_concurrent_runs: 8,
            cpu_load_throttle: Some(4.0),
        }
    }
}

/// Snapshot of *current* counter values the gate compares against. The
/// caller (la-daemon) computes these from the SQLite repos and the in-
/// memory "global running" counter; the gate does not query anything.
///
/// `window_cost_today` should treat NULL `cost_usd_est` rows as 0.
/// `running_for_cron` and `running_global` should count only in-flight
/// rows (`status IN ('pending','spawning','running') AND finished_at IS NULL`).
///
/// ## Audit-row counting contract
///
/// `window_runs_today` and `window_cost_today` MUST exclude rows the gate
/// itself wrote for quota refusals (`error_kind LIKE 'quota_%'`). If they
/// were included, a high-frequency cron that hit `max_runs_per_day` would
/// keep writing refusal audit rows on every subsequent tick, and each new
/// audit row would extend the window count above the cap — the cron would
/// be permanently denied even after the original admitted runs roll out
/// of the 24h window. The repo helper
/// `RunsRepo::count_since_for_cron` enforces this filter; do not bypass
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaSnapshot {
    pub running_for_cron: u32,
    pub running_global: u32,
    /// Admitted rows in the 24h rolling window for this cron (audit rows
    /// for quota refusals are excluded — see the contract above).
    pub window_runs_today: u32,
    /// Sum of `cost_usd_est` over the same admitted set.
    pub window_cost_today: f64,
    /// 1-min loadavg if obtainable on the host; `None` on Windows/unsupported.
    pub current_loadavg_1m: Option<f64>,
    /// Wall-clock "now" used by the backoff rail to decide whether the
    /// `failure_backoff` window has elapsed. Pass `Utc::now()` in production
    /// and a deterministic value in tests.
    pub now: DateTime<Utc>,
    /// Finished-at timestamp of the most recent terminal failure for this
    /// cron (`status IN ('failed','timed_out')`). `None` means the cron has
    /// never failed (or its last terminal status was a success / cancelled
    /// / budget_exceeded) — backoff rail is inactive in that case.
    pub last_terminal_failure_at: Option<DateTime<Utc>>,
}

/// What the gate decided. The caller decides what *side-effects* to take
/// based on this — typically: `Admit` → spawn; `RefuseBudgetExceeded`
/// → insert a `runs.status='budget_exceeded'` audit row; everything else
/// `RefuseXxx` → insert a `runs.status='cancelled'` audit row with the
/// reason tag from [`AdmissionDecision::error_kind`].
///
/// Two variants signal deferral rather than permanent refusal (see
/// [`AdmissionDecision::is_deferral`]): `RefuseDeferLoadavg` (the host is
/// overloaded right now) and `RefuseDeferBackoff` (the per-cron
/// `failure_backoff` window has not yet elapsed since the last terminal
/// failure). The caller MAY choose to skip writing audit rows for these
/// to avoid log spam, but the default `rejected_status` / `error_kind`
/// returned here will still produce a valid row if it does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AdmissionDecision {
    Admit,
    /// `enabled = false` on the cron row. Either the user disabled it or
    /// the consecutive-failures threshold tripped a previous run's
    /// auto-pause.
    RefusePaused,
    RefuseConcurrentPerCron {
        limit: u32,
        in_flight: u32,
    },
    RefuseConcurrentGlobal {
        limit: u32,
        in_flight: u32,
    },
    RefuseRunsPerDay {
        limit: u32,
        used: u32,
    },
    RefuseBudgetExceeded {
        limit_usd: f64,
        used_usd: f64,
    },
    RefuseDeferLoadavg {
        threshold: f64,
        sampled: f64,
    },
    /// `failure_backoff` rail: the most recent terminal failure plus the
    /// computed backoff delay falls in the future. `retry_after` is the
    /// wall-clock instant the gate will admit again, all else equal.
    RefuseDeferBackoff {
        retry_after: DateTime<Utc>,
        consecutive_failures: u32,
    },
}

impl AdmissionDecision {
    /// `true` for any non-admit variant.
    pub fn is_refusal(self) -> bool {
        !matches!(self, AdmissionDecision::Admit)
    }

    /// `true` when the gate intends a deferral rather than a permanent
    /// refusal. Today the loadavg and failure-backoff rails qualify —
    /// every other `Refuse*` is "this fire is dead, audit it".
    pub fn is_deferral(self) -> bool {
        matches!(
            self,
            AdmissionDecision::RefuseDeferLoadavg { .. }
                | AdmissionDecision::RefuseDeferBackoff { .. }
        )
    }

    /// Machine-readable reason tag for `runs.error_kind`. `None` for
    /// `Admit` because no row will be written by the gate (the run
    /// executor handles the success path).
    pub fn error_kind(self) -> Option<&'static str> {
        match self {
            AdmissionDecision::Admit => None,
            AdmissionDecision::RefusePaused => Some("quota_paused"),
            AdmissionDecision::RefuseConcurrentPerCron { .. } => Some("quota_max_concurrent_runs"),
            AdmissionDecision::RefuseConcurrentGlobal { .. } => {
                Some("quota_global_max_concurrent_runs")
            }
            AdmissionDecision::RefuseRunsPerDay { .. } => Some("quota_max_runs_per_day"),
            AdmissionDecision::RefuseBudgetExceeded { .. } => Some("quota_cost_budget_exceeded"),
            AdmissionDecision::RefuseDeferLoadavg { .. } => Some("quota_cpu_load_throttle"),
            AdmissionDecision::RefuseDeferBackoff { .. } => Some("quota_failure_backoff"),
        }
    }

    /// Status string suitable for [`la_storage::NewRejectedRun::status`]
    /// when the caller chooses to write an audit row.
    pub fn rejected_status(self) -> Option<&'static str> {
        match self {
            AdmissionDecision::Admit => None,
            AdmissionDecision::RefuseBudgetExceeded { .. } => Some("budget_exceeded"),
            // Every other refusal — paused, concurrency, runs/day, loadavg
            // deferral, backoff deferral — gets `cancelled` because the
            // schema's `status` enum only special-cases the cost-budget
            // path. The reason tag in `error_kind` lets the TUI
            // distinguish them.
            AdmissionDecision::RefusePaused
            | AdmissionDecision::RefuseConcurrentPerCron { .. }
            | AdmissionDecision::RefuseConcurrentGlobal { .. }
            | AdmissionDecision::RefuseRunsPerDay { .. }
            | AdmissionDecision::RefuseDeferLoadavg { .. }
            | AdmissionDecision::RefuseDeferBackoff { .. } => Some("cancelled"),
        }
    }

    /// Human-readable detail suitable for `runs.error_detail` and tracing.
    pub fn error_detail(self) -> String {
        match self {
            AdmissionDecision::Admit => String::new(),
            AdmissionDecision::RefusePaused => {
                "cron is disabled (likely auto-paused on consecutive failures)".into()
            }
            AdmissionDecision::RefuseConcurrentPerCron { limit, in_flight } => {
                format!("max_concurrent_runs={limit} reached (in_flight={in_flight})")
            }
            AdmissionDecision::RefuseConcurrentGlobal { limit, in_flight } => {
                format!("global_max_concurrent_runs={limit} reached (in_flight={in_flight})")
            }
            AdmissionDecision::RefuseRunsPerDay { limit, used } => {
                format!("max_runs_per_day={limit} reached (used={used})")
            }
            AdmissionDecision::RefuseBudgetExceeded {
                limit_usd,
                used_usd,
            } => {
                format!("cost_budget_usd_per_day={limit_usd:.4} exceeded (used={used_usd:.4})")
            }
            AdmissionDecision::RefuseDeferLoadavg { threshold, sampled } => {
                format!("cpu_load_throttle={threshold:.2} exceeded (loadavg_1m={sampled:.2})")
            }
            AdmissionDecision::RefuseDeferBackoff {
                retry_after,
                consecutive_failures,
            } => format!(
                "failure_backoff deferring until {} (consecutive_failures={consecutive_failures})",
                retry_after.to_rfc3339()
            ),
        }
    }
}

/// Evaluate a single fire against the per-cron quota, the global knobs,
/// and the current snapshot. The order of checks below is the priority
/// ordering — once we find a reason to refuse, we stop looking. The order
/// matters because the audit row only carries one reason tag; we surface
/// the most informative one for the operator.
///
/// 1. `enabled=false` — anything else is moot; a paused cron should not
///    fire. (Schema CHECK keeps `pause_on_consecutive_failures >= 1` so
///    the auto-pause path always has a non-zero threshold to compare.)
/// 2. `failure_backoff` — when the most recent terminal failure +
///    backoff delay is still in the future, defer; this is the §5.4
///    "连续失败时延后下一次触发" path. Evaluated before global/load
///    so backoff is visible even on an otherwise idle host.
/// 3. `global_max_concurrent_runs` — a blown global cap is a "stop the
///    world" signal; per-cron checks are downstream of it.
/// 4. `cpu_load_throttle` — defer (not cancel) when the host is under
///    load; we'd rather not even audit-spam in this case but the caller
///    chooses.
/// 5. `max_concurrent_runs` — per-cron concurrency.
/// 6. `max_runs_per_day` — per-cron 24h rolling cap.
/// 7. `cost_budget_usd_per_day` — circuit-breaker for adapter-reported
///    spend.
pub fn evaluate_admission(
    quota: &CronQuota,
    global: &GlobalQuota,
    snapshot: &QuotaSnapshot,
) -> AdmissionDecision {
    if !quota.enabled {
        return AdmissionDecision::RefusePaused;
    }

    if let Some(retry_after) = backoff_retry_after(quota, snapshot) {
        if retry_after > snapshot.now {
            return AdmissionDecision::RefuseDeferBackoff {
                retry_after,
                consecutive_failures: quota.consecutive_failures,
            };
        }
    }

    if global.global_max_concurrent_runs > 0
        && snapshot.running_global >= global.global_max_concurrent_runs
    {
        return AdmissionDecision::RefuseConcurrentGlobal {
            limit: global.global_max_concurrent_runs,
            in_flight: snapshot.running_global,
        };
    }

    if let (Some(threshold), Some(sampled)) =
        (global.cpu_load_throttle, snapshot.current_loadavg_1m)
    {
        if sampled > threshold {
            return AdmissionDecision::RefuseDeferLoadavg { threshold, sampled };
        }
    }

    if snapshot.running_for_cron >= quota.max_concurrent_runs {
        return AdmissionDecision::RefuseConcurrentPerCron {
            limit: quota.max_concurrent_runs,
            in_flight: snapshot.running_for_cron,
        };
    }

    if snapshot.window_runs_today >= quota.max_runs_per_day {
        return AdmissionDecision::RefuseRunsPerDay {
            limit: quota.max_runs_per_day,
            used: snapshot.window_runs_today,
        };
    }

    if let Some(limit_usd) = quota.cost_budget_usd_per_day {
        if limit_usd >= 0.0 && snapshot.window_cost_today >= limit_usd {
            return AdmissionDecision::RefuseBudgetExceeded {
                limit_usd,
                used_usd: snapshot.window_cost_today,
            };
        }
    }

    AdmissionDecision::Admit
}

/// Compute the wall-clock instant the backoff rail would next admit the
/// cron, or `None` if the rail is inactive (no backoff configured, no
/// prior failure, or zero consecutive failures). Public so the daemon's
/// status-bar layer can show "next eligible at …" without re-deriving.
pub fn backoff_retry_after(quota: &CronQuota, snapshot: &QuotaSnapshot) -> Option<DateTime<Utc>> {
    let backoff = quota.failure_backoff?;
    let last_failure = snapshot.last_terminal_failure_at?;
    if quota.consecutive_failures == 0 {
        return None;
    }
    let delay = backoff.delay_for(quota.consecutive_failures);
    if delay.is_zero() {
        return None;
    }
    let delta = chrono::Duration::from_std(delay).ok()?;
    last_failure.checked_add_signed(delta)
}

/// The architecture stores quotas on the `crons` row but the `max_runtime_s`
/// field is enforced *during* a run (timeout-kill), not at admission time.
/// We expose it here as a typed helper so the run executor doesn't have to
/// hand-convert. `max_runtime_s = 0` is treated as "no timeout" defensively
/// even though the schema CHECK requires `>= 1`.
pub fn max_runtime(quota: &CronQuota) -> Option<Duration> {
    if quota.max_runtime_s == 0 {
        None
    } else {
        Some(Duration::from_secs(u64::from(quota.max_runtime_s)))
    }
}

/// Decide whether the consecutive-failure threshold has been met. Called by
/// the run executor *after* it has bumped the counter on a terminal failure
/// and read back the new value. `threshold = 0` is treated as "never
/// auto-pause" defensively (schema CHECK requires `>= 1`).
pub fn should_auto_pause(threshold: u32, consecutive_failures_after_bump: u32) -> bool {
    threshold > 0 && consecutive_failures_after_bump >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn snap() -> QuotaSnapshot {
        QuotaSnapshot {
            running_for_cron: 0,
            running_global: 0,
            window_runs_today: 0,
            window_cost_today: 0.0,
            current_loadavg_1m: None,
            now: t0(),
            last_terminal_failure_at: None,
        }
    }

    fn quota() -> CronQuota {
        CronQuota::default()
    }

    fn global() -> GlobalQuota {
        // Disable loadavg by default so individual tests opt in.
        GlobalQuota {
            global_max_concurrent_runs: 8,
            cpu_load_throttle: None,
        }
    }

    #[test]
    fn admit_when_under_all_caps() {
        assert_eq!(
            evaluate_admission(&quota(), &global(), &snap()),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn paused_short_circuits_every_other_check() {
        let mut q = quota();
        q.enabled = false;
        // Even with a snapshot that would also blow per-day, we get RefusePaused.
        let mut s = snap();
        s.window_runs_today = 1_000_000;
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::RefusePaused
        );
    }

    #[test]
    fn global_concurrency_outranks_per_cron_concurrency() {
        let g = GlobalQuota {
            global_max_concurrent_runs: 2,
            cpu_load_throttle: None,
        };
        let mut s = snap();
        s.running_global = 2;
        s.running_for_cron = 1; // per-cron would still fit
        assert_eq!(
            evaluate_admission(&quota(), &g, &s),
            AdmissionDecision::RefuseConcurrentGlobal {
                limit: 2,
                in_flight: 2,
            }
        );
    }

    #[test]
    fn global_concurrency_disabled_when_zero() {
        let g = GlobalQuota {
            global_max_concurrent_runs: 0,
            cpu_load_throttle: None,
        };
        let mut s = snap();
        s.running_global = 1_000;
        // Skipped — drops through and admits because per-cron limit (1) is
        // not yet hit (running_for_cron is still 0).
        assert_eq!(
            evaluate_admission(&quota(), &g, &s),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn loadavg_defers_when_above_threshold() {
        let g = GlobalQuota {
            global_max_concurrent_runs: 0, // skip the global concurrency rail
            cpu_load_throttle: Some(4.0),
        };
        let mut s = snap();
        s.current_loadavg_1m = Some(5.5);
        let decision = evaluate_admission(&quota(), &g, &s);
        assert!(matches!(
            decision,
            AdmissionDecision::RefuseDeferLoadavg { .. }
        ));
        assert!(decision.is_deferral());
    }

    #[test]
    fn loadavg_skipped_when_sample_unavailable() {
        let g = GlobalQuota {
            global_max_concurrent_runs: 0,
            cpu_load_throttle: Some(0.5), // very strict; would always trip if sampled
        };
        // sample = None → cannot evaluate → drops through to Admit.
        assert_eq!(
            evaluate_admission(&quota(), &g, &snap()),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn per_cron_concurrency_caps_at_limit() {
        let mut s = snap();
        s.running_for_cron = 1; // default max_concurrent_runs = 1
        assert_eq!(
            evaluate_admission(&quota(), &global(), &s),
            AdmissionDecision::RefuseConcurrentPerCron {
                limit: 1,
                in_flight: 1,
            }
        );
    }

    #[test]
    fn runs_per_day_refuses_at_and_above_limit() {
        let mut q = quota();
        q.max_runs_per_day = 2;
        let mut s = snap();
        s.window_runs_today = 2;
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::RefuseRunsPerDay { limit: 2, used: 2 }
        );
    }

    #[test]
    fn cost_budget_refuses_at_or_above_limit() {
        let mut q = quota();
        q.cost_budget_usd_per_day = Some(1.0);
        let mut s = snap();
        s.window_cost_today = 1.0;
        assert!(matches!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::RefuseBudgetExceeded { .. }
        ));
        s.window_cost_today = 1.5;
        assert!(matches!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::RefuseBudgetExceeded { .. }
        ));
    }

    #[test]
    fn cost_budget_none_means_unbounded() {
        let mut q = quota();
        q.cost_budget_usd_per_day = None;
        let mut s = snap();
        s.window_cost_today = 1_000_000.0;
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn combined_multiple_caps_picks_highest_priority_refusal() {
        // Per-cron concurrency AND per-day AND budget all blown; the
        // expected refusal is per-cron concurrency (it's earlier in the
        // priority order than per-day; global concurrency is even earlier
        // but we don't blow it here).
        let mut q = quota();
        q.max_runs_per_day = 1;
        q.cost_budget_usd_per_day = Some(0.5);
        let mut s = snap();
        s.running_for_cron = 1;
        s.window_runs_today = 5;
        s.window_cost_today = 9.0;
        assert!(matches!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::RefuseConcurrentPerCron { .. }
        ));
    }

    #[test]
    fn error_kind_and_status_round_trip() {
        for decision in [
            AdmissionDecision::RefusePaused,
            AdmissionDecision::RefuseConcurrentPerCron {
                limit: 1,
                in_flight: 1,
            },
            AdmissionDecision::RefuseConcurrentGlobal {
                limit: 8,
                in_flight: 8,
            },
            AdmissionDecision::RefuseRunsPerDay { limit: 2, used: 2 },
            AdmissionDecision::RefuseBudgetExceeded {
                limit_usd: 1.0,
                used_usd: 1.5,
            },
            AdmissionDecision::RefuseDeferLoadavg {
                threshold: 4.0,
                sampled: 5.0,
            },
            AdmissionDecision::RefuseDeferBackoff {
                retry_after: t0(),
                consecutive_failures: 3,
            },
        ] {
            assert!(decision.is_refusal());
            assert!(decision.error_kind().is_some());
            assert!(decision.rejected_status().is_some());
            assert!(!decision.error_detail().is_empty());
        }
        let admit = AdmissionDecision::Admit;
        assert!(!admit.is_refusal());
        assert!(admit.error_kind().is_none());
        assert!(admit.rejected_status().is_none());
        assert!(admit.error_detail().is_empty());
    }

    #[test]
    fn backoff_rail_defers_inside_window_and_admits_once_past_it() {
        // 60s base, 2x factor, 1h cap; after 1 failure → 60s window.
        let mut q = quota();
        q.consecutive_failures = 1;
        let mut s = snap();
        s.last_terminal_failure_at = Some(t0());

        // 30s after the failure: still inside the 60s window → defer.
        s.now = t0() + chrono::Duration::seconds(30);
        let decision = evaluate_admission(&q, &global(), &s);
        match decision {
            AdmissionDecision::RefuseDeferBackoff {
                retry_after,
                consecutive_failures,
            } => {
                assert_eq!(retry_after, t0() + chrono::Duration::seconds(60));
                assert_eq!(consecutive_failures, 1);
                assert!(decision.is_deferral());
            }
            other => panic!("expected RefuseDeferBackoff, got {other:?}"),
        }

        // 60s after the failure: window edge — admitted.
        s.now = t0() + chrono::Duration::seconds(60);
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::Admit
        );

        // 90s after: well past the window → admitted.
        s.now = t0() + chrono::Duration::seconds(90);
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn backoff_grows_exponentially_across_consecutive_failures() {
        // failure 1 → 60s, failure 2 → 120s, failure 6 → 1920s
        let mut q = quota();
        let mut s = snap();
        s.last_terminal_failure_at = Some(t0());

        q.consecutive_failures = 2;
        s.now = t0() + chrono::Duration::seconds(119);
        assert!(matches!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::RefuseDeferBackoff { .. }
        ));
        s.now = t0() + chrono::Duration::seconds(120);
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::Admit
        );

        q.consecutive_failures = 6;
        s.now = t0() + chrono::Duration::seconds(1919);
        assert!(matches!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::RefuseDeferBackoff { .. }
        ));
        s.now = t0() + chrono::Duration::seconds(1920);
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn backoff_inactive_when_no_failure_or_no_config() {
        let mut s = snap();
        s.last_terminal_failure_at = Some(t0());
        // consecutive_failures = 0 → rail inactive even after a failure
        // (means the last terminal status was non-failure; reset happened).
        let q = quota();
        s.now = t0();
        assert_eq!(
            evaluate_admission(&q, &global(), &s),
            AdmissionDecision::Admit
        );

        // failure_backoff = None → rail inactive
        let q_no_backoff = CronQuota {
            failure_backoff: None,
            consecutive_failures: 99,
            ..CronQuota::default()
        };
        s.now = t0() + chrono::Duration::seconds(1);
        assert_eq!(
            evaluate_admission(&q_no_backoff, &global(), &s),
            AdmissionDecision::Admit
        );

        // last_terminal_failure_at = None → rail inactive even with counter > 0
        let q_pending = CronQuota {
            consecutive_failures: 99,
            ..CronQuota::default()
        };
        let mut s2 = snap();
        s2.last_terminal_failure_at = None;
        s2.now = t0() + chrono::Duration::seconds(1);
        assert_eq!(
            evaluate_admission(&q_pending, &global(), &s2),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn backoff_evaluated_before_global_concurrency_and_loadavg() {
        // Configure backoff to defer, AND blow global concurrency AND
        // loadavg — the refusal we surface is the backoff one because it
        // ranks higher in priority order.
        let mut q = quota();
        q.consecutive_failures = 1;
        let g = GlobalQuota {
            global_max_concurrent_runs: 1,
            cpu_load_throttle: Some(4.0),
        };
        let mut s = snap();
        s.last_terminal_failure_at = Some(t0());
        s.now = t0() + chrono::Duration::seconds(10);
        s.running_global = 5;
        s.current_loadavg_1m = Some(99.0);
        assert!(matches!(
            evaluate_admission(&q, &g, &s),
            AdmissionDecision::RefuseDeferBackoff { .. }
        ));
    }

    #[test]
    fn auto_pause_threshold_inclusive() {
        assert!(!should_auto_pause(0, 100)); // 0 disables
        assert!(!should_auto_pause(5, 4));
        assert!(should_auto_pause(5, 5));
        assert!(should_auto_pause(5, 6));
    }

    #[test]
    fn max_runtime_helper() {
        let q = CronQuota::default();
        assert_eq!(max_runtime(&q), Some(Duration::from_secs(1800)));
        let q0 = CronQuota {
            max_runtime_s: 0,
            ..CronQuota::default()
        };
        assert_eq!(max_runtime(&q0), None);
    }

    #[test]
    fn backoff_retry_after_matches_evaluate_decision() {
        let mut q = quota();
        q.consecutive_failures = 3;
        let mut s = snap();
        s.last_terminal_failure_at = Some(t0());
        // 3 failures with default expo(1m,2,1h): 60 * 2^(3-1) = 240s
        let expected = t0() + chrono::Duration::seconds(240);
        assert_eq!(backoff_retry_after(&q, &s), Some(expected));
        s.now = expected - chrono::Duration::seconds(1);
        match evaluate_admission(&q, &global(), &s) {
            AdmissionDecision::RefuseDeferBackoff { retry_after, .. } => {
                assert_eq!(retry_after, expected);
            }
            other => panic!("expected backoff defer, got {other:?}"),
        }
    }
}
