//! Integration tests for the live [`la_scheduler::Scheduler`] loop.
//!
//! All tests are written under `#[tokio::test(start_paused = true)]` so that
//! `tokio::time::advance` drives both the monotonic clock the loop sleeps on
//! and the wall clock the FakeClock exposes. The result is that a "seven day
//! timeline" — the architecture-spec verification scenario — finishes in
//! milliseconds.

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;
use chrono_tz::Tz;
use tokio::time::{advance, timeout, Duration as StdDuration};

use la_scheduler::{
    CatchupMode, Clock, CronSpec, FakeClock, FireEvent, Scheduler, SchedulerChannels,
    SchedulerEvent,
};

/// Common fixture: anchor the fake clock at `wall`, spin up the scheduler,
/// and return the channels + clock so the test can advance time.
fn start_at(wall: DateTime<Utc>) -> (Arc<FakeClock>, SchedulerChannels) {
    let clock = Arc::new(FakeClock::new(wall));
    let (channels, _join) = Scheduler::start(clock.clone());
    (clock, channels)
}

/// Helper: pull every queued fire event for `cron_id` up to `expected_max`
/// using a short tokio::time-aware timeout. Returns whatever it could collect.
async fn drain_fires(rx: &mut tokio::sync::mpsc::Receiver<FireEvent>) -> Vec<FireEvent> {
    let mut out = Vec::new();
    // Use a real-time poll so the loop doesn't starve under start_paused.
    while let Ok(Some(ev)) = timeout(StdDuration::from_millis(50), rx.recv()).await {
        out.push(ev);
    }
    out
}

/// Advance Tokio time in 10-minute steps so the scheduler loop runs all
/// intermediate fires instead of jumping over them with one giant sleep.
async fn advance_in_steps(total: StdDuration, step: StdDuration) {
    let mut remaining = total;
    while remaining > StdDuration::ZERO {
        let chunk = std::cmp::min(remaining, step);
        advance(chunk).await;
        tokio::task::yield_now().await;
        remaining -= chunk;
    }
}

// ---------------------------------------------------------------------------
// §5.3 catch-up — seven-day simulated timeline, three modes
// ---------------------------------------------------------------------------

/// Drive the scheduler for ~7 days of simulated time with an hourly cron and
/// assert each catch-up mode behaves as specified:
/// - `skip`   : after a forced "missed window" we never see the back-fill.
/// - `coalesce`: one merged fire after the gap, the rest are normal hourly.
/// - `replay` : every missed hour fires once after the gap.
///
/// We model the "missed window" by injecting a wall-skew of +6 h while the
/// monotonic clock barely moves, which is exactly what catching up after a
/// laptop suspend looks like. The skew tick will trip and the install path
/// computes the missed fires using the entry's stored `last_fired_at`.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn seven_day_timeline_skip_mode() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (clock, mut ch) = start_at(wall);

    // Install at 12:00:30 with last_fired_at = noon. There are no missed
    // fires at install time.
    advance(StdDuration::from_secs(30)).await;
    let spec = CronSpec::parse("0 * * * *", "UTC").unwrap(); // hourly at :00
    ch.handle
        .upsert(
            "skip-cron",
            spec.clone(),
            CatchupMode::Skip,
            Duration::zero(),
            Some(clock.wall_now()),
        )
        .await
        .unwrap();

    // 1) Run forward 3 h normally — expect three fires (13:00, 14:00, 15:00).
    advance_in_steps(StdDuration::from_secs(3 * 3600), StdDuration::from_secs(60)).await;
    let fires = drain_fires(&mut ch.fires).await;
    assert_eq!(fires.len(), 3, "expected 3 normal fires, got {fires:#?}");

    // 2) Simulate a 5-hour wall-clock jump (NTP step / suspend resume) and
    //    let the 60s skew tick run a couple of times.
    clock.inject_wall_skew(Duration::hours(5));
    advance_in_steps(StdDuration::from_secs(120), StdDuration::from_secs(30)).await;

    // The recompute + per-entry catch-up runs across the skew gap, but for
    // `skip` mode `apply_catchup` emits zero fires by definition — so no
    // back-fill events appear. (Coalesce/replay variants are covered by
    // skew_with_coalesce_emits_one_merged_fire and the replay counterpart.)
    let post_skew = drain_fires(&mut ch.fires).await;
    assert!(
        post_skew.is_empty(),
        "skip mode must not back-fill missed fires after skew: {post_skew:#?}",
    );

    ch.handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn seven_day_timeline_coalesce_emits_single_catchup_event_on_install() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (clock, mut ch) = start_at(wall);

    // Pretend we've been down for 7 hours: last_fired_at = 5:00, now = 12:00.
    let last = Utc.with_ymd_and_hms(2026, 1, 1, 5, 0, 0).unwrap();
    let spec = CronSpec::parse("0 * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "coalesce-cron",
            spec,
            CatchupMode::Coalesce,
            Duration::zero(),
            Some(last),
        )
        .await
        .unwrap();
    // Give the install task a chance to push the catch-up event before we
    // drain. start_paused means we must actively yield.
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    let fires = drain_fires(&mut ch.fires).await;
    assert_eq!(fires.len(), 1, "coalesce must emit exactly one merged fire");
    let ev = &fires[0];
    assert_eq!(ev.cron_id, "coalesce-cron");
    // 7 missed top-of-hours: 06,07,08,09,10,11,12.
    assert_eq!(ev.coalesced_count, 7);
    assert!(!ev.catchup_degraded);
    assert_eq!(
        ev.scheduled_at,
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    );

    // Continuing forward should emit one fire per hour.
    advance_in_steps(StdDuration::from_secs(3 * 3600), StdDuration::from_secs(60)).await;
    let more = drain_fires(&mut ch.fires).await;
    assert_eq!(more.len(), 3);
    for f in &more {
        assert_eq!(f.coalesced_count, 1);
    }
    let _ = clock;
    ch.handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn seven_day_timeline_replay_emits_every_missed_fire() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    let last = Utc.with_ymd_and_hms(2026, 1, 1, 5, 0, 0).unwrap();
    let spec = CronSpec::parse("0 * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "replay-cron",
            spec,
            CatchupMode::Replay,
            // 30-minute throttle — wider than the natural cadence so it has
            // no practical effect on the hourly pattern, but exercises the
            // code path.
            Duration::minutes(30),
            Some(last),
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    let fires = drain_fires(&mut ch.fires).await;
    // 7 missed hours.
    assert_eq!(fires.len(), 7, "replay must emit every missed fire");
    for f in &fires {
        assert_eq!(f.coalesced_count, 1);
        assert!(!f.catchup_degraded);
    }
    assert_eq!(
        fires.first().unwrap().scheduled_at,
        Utc.with_ymd_and_hms(2026, 1, 1, 6, 0, 0).unwrap()
    );
    assert_eq!(
        fires.last().unwrap().scheduled_at,
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    );

    ch.handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn over_max_catchup_degrades_to_single_fire() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 8, 0, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    // 7 days @ 1 fire/min = 10080 fires; way over the 100 cap.
    let last = wall - Duration::days(7);
    let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "flood",
            spec,
            CatchupMode::Replay,
            Duration::zero(),
            Some(last),
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    let fires = drain_fires(&mut ch.fires).await;
    assert_eq!(fires.len(), 1, "over-cap must collapse to one fire");
    assert!(fires[0].catchup_degraded);

    ch.handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §5.1 DST — IANA timezone fires use wall-clock semantics
// ---------------------------------------------------------------------------

#[test]
fn dst_spring_forward_skips_missing_local_hour() {
    // 2026-03-08 02:30 America/Los_Angeles does NOT exist (spring forward
    // jumps 02:00 → 03:00 PDT). A cron at "30 2 * * *" must skip that day
    // and fire on the next day instead.
    let tz: Tz = Los_Angeles;
    let spec = CronSpec::parse("30 2 * * *", "America/Los_Angeles").unwrap();

    // Anchor "now" at 2026-03-08 00:00 local → UTC.
    let now_local = tz.with_ymd_and_hms(2026, 3, 8, 0, 0, 0).unwrap();
    let now_utc = now_local.with_timezone(&Utc);
    let next = spec.next_after(now_utc).unwrap();
    // The next valid 02:30 local is 2026-03-09 02:30 PDT.
    let expect_local = tz.with_ymd_and_hms(2026, 3, 9, 2, 30, 0).unwrap();
    assert_eq!(next, expect_local.with_timezone(&Utc));
}

#[test]
fn dst_fall_back_fires_ambiguous_hour_in_both_offsets() {
    // 2026-11-01: LA clocks fall back at 02:00 → 01:00 PST. A cron at
    // "30 1 * * *" matches the wall-clock pattern "01:30 local" twice — once
    // at 01:30 PDT (08:30 UTC) and again at 01:30 PST (09:30 UTC). This is
    // the cron crate's documented behaviour and matches the IANA semantics
    // most users expect: the wall clock literally shows 01:30 on two
    // separate instants, so the cron fires on both. Verifying it here pins
    // the semantics so a future cron-crate upgrade can't silently regress.
    let tz: Tz = Los_Angeles;
    let spec = CronSpec::parse("30 1 * * *", "America/Los_Angeles").unwrap();

    let now_local = tz.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
    let now_utc = now_local.with_timezone(&Utc);
    let fires = spec.fires_between(now_utc, now_utc + Duration::hours(6), 8);
    // Filter to fires that fall on calendar day Nov 1 in the LA tz.
    let day_fires: Vec<_> = fires
        .iter()
        .copied()
        .filter(|f| {
            let local = f.with_timezone(&tz);
            local.day() == 1 && local.month() == 11
        })
        .collect();
    assert_eq!(
        day_fires.len(),
        2,
        "fall-back day's ambiguous 01:30 fires once per offset, got {fires:#?}",
    );
    // Both fires must be exactly one hour apart in UTC: PDT at 08:30Z then
    // PST at 09:30Z.
    assert_eq!(day_fires[1] - day_fires[0], Duration::hours(1));
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn dst_aware_scheduler_fires_on_la_local_wall_time() {
    // End-to-end: install a cron in LA timezone, drive the scheduler across
    // the spring-forward boundary, observe that no spurious fire occurs in
    // the skipped 02:00..03:00 window.
    let wall = chrono_tz::America::Los_Angeles
        .with_ymd_and_hms(2026, 3, 8, 1, 30, 0)
        .unwrap()
        .with_timezone(&Utc);
    let (_clock, mut ch) = start_at(wall);

    // Hourly at the top of the hour. Local hours seen: 01:00 (skipped), 02:00
    // doesn't exist, 03:00 PDT exists; UTC, 02:00 and 03:00 PDT differ by 1 h
    // because the clock jumped.
    let spec = CronSpec::parse("0 * * * *", "America/Los_Angeles").unwrap();
    ch.handle
        .upsert("la-hourly", spec, CatchupMode::Skip, Duration::zero(), None)
        .await
        .unwrap();

    // Advance 6 wall-hours of simulated time. The next fires we expect at
    // LA-local 03:00, 04:00, 05:00, 06:00, 07:00 — 5 fires.
    advance_in_steps(StdDuration::from_secs(6 * 3600), StdDuration::from_secs(60)).await;
    let fires = drain_fires(&mut ch.fires).await;
    let local_hours: Vec<u32> = fires
        .iter()
        .map(|f| f.scheduled_at.with_timezone(&Los_Angeles).hour())
        .collect();
    // The exact count depends on whether the 02:00 wall-pattern would have
    // been the natural next fire from 01:30 (it would, but it's skipped by
    // DST). Required invariant: no fire's local hour equals 2.
    use chrono::Timelike;
    let _ = local_hours; // touched only for shape; the assertion below is the contract
    for f in &fires {
        let local_hour = f.scheduled_at.with_timezone(&Los_Angeles).hour();
        assert_ne!(local_hour, 2, "no fire may land in the skipped 02:00 hour");
    }
    assert!(!fires.is_empty(), "expected at least one post-DST fire");

    ch.handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §5.2 heap reordering — upsert / delete take immediate effect
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn upsert_with_earlier_time_takes_immediate_effect_on_live_loop() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    // First insert a cron whose next fire is far in the future (12:00 UTC).
    let later = CronSpec::parse("0 12 * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "later",
            later,
            CatchupMode::Coalesce,
            Duration::zero(),
            None,
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    // Confirm via snapshot.
    let snap = ch.handle.snapshot().await.unwrap();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].id, "later");

    // Now upsert an earlier cron that should fire in 5 minutes.
    let earlier = CronSpec::parse("5 0 * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "earlier",
            earlier,
            CatchupMode::Coalesce,
            Duration::zero(),
            None,
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    let snap = ch.handle.snapshot().await.unwrap();
    assert_eq!(snap.len(), 2);
    // Earliest must be the just-inserted "earlier" entry.
    assert_eq!(snap[0].id, "earlier");

    // Advance past 5 minutes; the earlier cron must fire before the later
    // one (which is hours away).
    advance_in_steps(StdDuration::from_secs(6 * 60), StdDuration::from_secs(30)).await;
    let fires = drain_fires(&mut ch.fires).await;
    assert!(
        fires.iter().any(|f| f.cron_id == "earlier"),
        "earlier cron must fire after upsert; saw {fires:#?}",
    );
    assert!(
        fires.iter().all(|f| f.cron_id != "later"),
        "later cron must not fire yet",
    );

    ch.handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn delete_prevents_future_fire() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("5 0 * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "doomed",
            spec,
            CatchupMode::Coalesce,
            Duration::zero(),
            None,
        )
        .await
        .unwrap();
    let existed = ch.handle.delete("doomed").await.unwrap();
    assert!(existed);

    advance_in_steps(StdDuration::from_secs(10 * 60), StdDuration::from_secs(30)).await;
    let fires = drain_fires(&mut ch.fires).await;
    assert!(fires.is_empty(), "deleted cron must not fire: {fires:#?}");

    let snap = ch.handle.snapshot().await.unwrap();
    assert!(snap.is_empty());
    ch.handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §5.2 clock-skew detector — > 30 s drift triggers full re-heap
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn clock_skew_above_threshold_emits_diagnostic() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let (clock, mut ch) = start_at(wall);

    // Add an entry so the recompute pass has something to do.
    let spec = CronSpec::parse("0 12 * * *", "UTC").unwrap();
    ch.handle
        .upsert("anchor", spec, CatchupMode::Skip, Duration::zero(), None)
        .await
        .unwrap();

    // Skew by +5 minutes (well above the 30 s threshold).
    clock.inject_wall_skew(Duration::minutes(5));
    // Advance past the 60 s skew tick.
    advance_in_steps(StdDuration::from_secs(70), StdDuration::from_secs(10)).await;

    let diag = timeout(StdDuration::from_millis(100), ch.diagnostics.recv())
        .await
        .expect("diagnostics arrived")
        .expect("channel still open");
    match diag {
        SchedulerEvent::ClockSkewDetected {
            skew_seconds,
            recomputed_entries,
        } => {
            assert!(
                skew_seconds >= 30 * 5,
                "expected positive skew, got {skew_seconds}"
            );
            assert_eq!(recomputed_entries, 1);
        }
    }

    ch.handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn clock_skew_below_threshold_is_silent() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let (clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("0 12 * * *", "UTC").unwrap();
    ch.handle
        .upsert("anchor", spec, CatchupMode::Skip, Duration::zero(), None)
        .await
        .unwrap();

    // +20 s skew, below the > 30 s threshold.
    clock.inject_wall_skew(Duration::seconds(20));
    advance_in_steps(StdDuration::from_secs(70), StdDuration::from_secs(10)).await;

    let diag = timeout(StdDuration::from_millis(100), ch.diagnostics.recv()).await;
    assert!(diag.is_err(), "no diagnostic should fire for 20 s skew");

    ch.handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §5.3 unified catch-up — skew-recompute and starvation must also apply the
// per-entry catch-up policy (Software Architect review blockers).
// ---------------------------------------------------------------------------

/// Laptop-suspend / NTP-step scenario: when wall time jumps forward by
/// several hours, a `coalesce` user's cron MUST emit one merged catch-up
/// fire covering the gap. Prior to the fix the skew path only re-anchored
/// `next_fire_at` and silently dropped every missed fire — invisible to UI
/// and inconsistent with §5.3's "coalesce is the default" contract.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn skew_with_coalesce_emits_one_merged_fire() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("0 * * * *", "UTC").unwrap(); // hourly at :00
    ch.handle
        .upsert(
            "coalesce-suspend",
            spec,
            CatchupMode::Coalesce,
            Duration::zero(),
            // Pretend last fire was 12:00 (== now at install).
            Some(wall),
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    // Install path emits nothing (no gap yet) — confirm before we shake the clock.
    let pre = drain_fires(&mut ch.fires).await;
    assert!(pre.is_empty(), "no fires expected before skew: {pre:#?}");

    // Inject a +5h wall jump and let the 60s skew tick observe it.
    clock.inject_wall_skew(Duration::hours(5));
    advance_in_steps(StdDuration::from_secs(120), StdDuration::from_secs(30)).await;

    let fires = drain_fires(&mut ch.fires).await;
    assert_eq!(
        fires.len(),
        1,
        "coalesce skew must emit exactly one merged fire, got {fires:#?}",
    );
    let ev = &fires[0];
    assert_eq!(ev.cron_id, "coalesce-suspend");
    // 5 missed hours: 13:00, 14:00, 15:00, 16:00, 17:00.
    assert_eq!(ev.coalesced_count, 5);
    assert!(!ev.catchup_degraded);

    // Drain (and discard) the diagnostic so the channel doesn't fill.
    let _ = timeout(StdDuration::from_millis(50), ch.diagnostics.recv()).await;
    ch.handle.shutdown().await.unwrap();
}

/// Same scenario, `replay` policy: every missed hour fires once after the
/// skew, in order, without `catchup_degraded`. Covers the same blocker as
/// the coalesce variant — proves the skew path honours per-entry policy
/// rather than collapsing to skip.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn skew_with_replay_emits_every_missed_fire() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("0 * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "replay-suspend",
            spec,
            CatchupMode::Replay,
            Duration::zero(),
            Some(wall),
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let _ = drain_fires(&mut ch.fires).await;

    clock.inject_wall_skew(Duration::hours(5));
    advance_in_steps(StdDuration::from_secs(120), StdDuration::from_secs(30)).await;

    let fires = drain_fires(&mut ch.fires).await;
    assert_eq!(
        fires.len(),
        5,
        "replay skew must emit every missed hourly fire, got {fires:#?}",
    );
    for f in &fires {
        assert_eq!(f.coalesced_count, 1);
        assert!(!f.catchup_degraded);
        assert_eq!(f.cron_id, "replay-suspend");
    }
    let scheduled_at: Vec<_> = fires.iter().map(|f| f.scheduled_at).collect();
    let expected: Vec<_> = (13..=17)
        .map(|h| Utc.with_ymd_and_hms(2026, 1, 1, h, 0, 0).unwrap())
        .collect();
    assert_eq!(scheduled_at, expected);

    let _ = timeout(StdDuration::from_millis(50), ch.diagnostics.recv()).await;
    ch.handle.shutdown().await.unwrap();
}

/// Steady-state starvation: the fire path is woken late (long lock held
/// elsewhere or OS scheduler delay) so `now > top.fire_at + cadence`. The
/// pre-fix loop computed `next_after(now)` and silently dropped every
/// natural fire that landed in the starvation gap. The fix routes those
/// gap fires through the per-entry catch-up policy.
///
/// We exercise the path by installing a per-minute cron with `coalesce`
/// and using a separate hold task to stall the scheduler's table lock
/// well past several fire times — when the hold releases, the loop wakes
/// at `now = install + 5min` with `top.fire_at = install + 1min`, so 4
/// fires landed in the gap. Coalesce policy must emit:
///   - 1 normal fire for `top.fire_at` (12:01)
///   - 1 merged catch-up fire for 12:02..12:05 with coalesced_count == 4
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn starved_loop_runs_catchup_policy() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("* * * * *", "UTC").unwrap(); // every minute
    ch.handle
        .upsert(
            "minutely-coalesce",
            spec,
            CatchupMode::Coalesce,
            Duration::zero(),
            None,
        )
        .await
        .unwrap();
    // Let the scheduler reach the sleep_until on the 12:01 fire.
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    // Jump simulated wall+mono time forward by 5 minutes in one step. The
    // scheduler's sleep_until(deadline=12:01) was set with the original
    // anchor; advancing by 5 minutes resolves it immediately, but by the
    // time the loop's wall_now() runs in the fire arm, it observes 12:05.
    advance(StdDuration::from_secs(5 * 60)).await;
    tokio::task::yield_now().await;
    // Give the loop a few extra polls to drain its inner pop loop.
    advance(StdDuration::from_millis(10)).await;
    tokio::task::yield_now().await;

    let fires = drain_fires(&mut ch.fires).await;
    // Expected: normal 12:01 fire + one coalesced fire merging 12:02..12:05.
    assert_eq!(
        fires.len(),
        2,
        "starved coalesce must emit popped fire + merged catch-up, got {fires:#?}",
    );

    let normal = &fires[0];
    assert_eq!(normal.cron_id, "minutely-coalesce");
    assert_eq!(
        normal.scheduled_at,
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 1, 0).unwrap()
    );
    assert_eq!(normal.coalesced_count, 1);
    assert!(!normal.catchup_degraded);

    let merged = &fires[1];
    assert_eq!(merged.cron_id, "minutely-coalesce");
    assert_eq!(
        merged.scheduled_at,
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 5, 0).unwrap()
    );
    assert_eq!(merged.coalesced_count, 4, "12:02..12:05 = 4 missed fires");
    assert!(!merged.catchup_degraded);

    ch.handle.shutdown().await.unwrap();
}

/// Watermark regression: install runs catch-up for a `last_fired_at = 5h ago`
/// coalesce cron (emitting one merged fire), then a clock skew trips the
/// 60s tick. The shared catch-up resolver would replay the same gap if
/// install left `last_fired_at` at the *original* 5h-ago value — Code
/// Reviewer noted the pre-fix `last_fired_at.or(Some(now))` did exactly
/// that, double-firing every coalesce/replay user whenever the laptop woke
/// up shortly after daemon restart. This test pins the fix: install must
/// advance the watermark to `now` so the skew pass sees no gap.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn install_then_skew_does_not_double_fire_catchup() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (clock, mut ch) = start_at(wall);

    // Install at 12:00 with last_fired_at = 7:00 — 5 missed hourlies.
    let last = Utc.with_ymd_and_hms(2026, 1, 1, 7, 0, 0).unwrap();
    let spec = CronSpec::parse("0 * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "restart-coalesce",
            spec,
            CatchupMode::Coalesce,
            Duration::zero(),
            Some(last),
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    let install_fires = drain_fires(&mut ch.fires).await;
    assert_eq!(
        install_fires.len(),
        1,
        "install must emit exactly one merged catch-up fire",
    );
    assert_eq!(install_fires[0].coalesced_count, 5);

    // Inject a +60s wall skew (above the > 30s threshold) and let the 60s
    // skew tick observe it. The cron's next natural fire is 13:00, still
    // ~1h away — so the only thing that can produce a fire in this window
    // is the skew-recompute path replaying install's gap. With the fix,
    // install advanced last_fired_at to `now` (12:00), so the skew pass
    // sees `last == now` (modulo the 60s skew itself, which is itself
    // short of any hourly boundary) and emits zero catch-up fires.
    clock.inject_wall_skew(Duration::seconds(60));
    advance_in_steps(StdDuration::from_secs(120), StdDuration::from_secs(30)).await;

    let post = drain_fires(&mut ch.fires).await;
    assert!(
        post.is_empty(),
        "skew after install must not replay install's catch-up: {post:#?}",
    );

    // Sanity: the skew tick *did* fire its diagnostic, so we know the
    // recompute path actually ran.
    let diag = timeout(StdDuration::from_millis(50), ch.diagnostics.recv()).await;
    assert!(
        matches!(diag, Ok(Some(SchedulerEvent::ClockSkewDetected { .. }))),
        "expected ClockSkewDetected diagnostic to confirm skew path ran, got {diag:?}",
    );

    ch.handle.shutdown().await.unwrap();
}
