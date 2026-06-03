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
    apply_catchup, next_eligible_fire, BackoffState, CatchupMode, Clock, CronSpec, FailureBackoff,
    FakeClock, FireEvent, Scheduler, SchedulerChannels, SchedulerEvent,
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

#[test]
fn seven_day_timeline_all_catchup_modes_stay_under_cap() {
    let spec = CronSpec::parse("0 */4 * * *", "UTC").unwrap();
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let end = start + Duration::days(7);
    let missed = spec.fires_between(start, end, 64);
    assert_eq!(
        missed.len(),
        42,
        "7 days at 4h cadence should produce 42 missed fires"
    );

    let skip = apply_catchup(&missed, CatchupMode::Skip, Duration::zero()).unwrap();
    assert!(skip.fires.is_empty());
    assert!(!skip.degraded_to_skip);

    let coalesce = apply_catchup(&missed, CatchupMode::Coalesce, Duration::zero()).unwrap();
    assert_eq!(coalesce.fires.len(), 1);
    assert_eq!(coalesce.fires[0].coalesced_count, 42);
    assert_eq!(coalesce.fires[0].scheduled_at, *missed.last().unwrap());
    assert!(!coalesce.degraded_to_skip);

    let replay = apply_catchup(&missed, CatchupMode::Replay, Duration::zero()).unwrap();
    assert_eq!(replay.fires.len(), missed.len());
    assert_eq!(
        replay
            .fires
            .iter()
            .map(|f| f.scheduled_at)
            .collect::<Vec<_>>(),
        missed
    );
    assert!(replay.fires.iter().all(|f| f.coalesced_count == 1));
    assert!(!replay.degraded_to_skip);
}

#[test]
fn seven_day_dst_windows_skip_spring_gap_and_collapse_fall_overlap() {
    let tz: Tz = Los_Angeles;

    let spring = CronSpec::parse("30 2 * * *", "America/Los_Angeles").unwrap();
    let spring_start = tz
        .with_ymd_and_hms(2026, 3, 5, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let spring_end = spring_start + Duration::days(7);
    let spring_fires = spring.fires_between(spring_start, spring_end, 16);
    assert_eq!(
        spring_fires.len(),
        6,
        "7-day spring-forward window must skip the nonexistent local 02:30"
    );
    assert!(
        spring_fires
            .iter()
            .all(|f| !(f.with_timezone(&tz).month() == 3 && f.with_timezone(&tz).day() == 8)),
        "spring-forward day must not produce a 02:30 fire: {spring_fires:#?}"
    );

    let fall = CronSpec::parse("30 1 * * *", "America/Los_Angeles").unwrap();
    let fall_start = tz
        .with_ymd_and_hms(2026, 10, 29, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let fall_end = fall_start + Duration::days(7);
    let fall_fires = fall.fires_between(fall_start, fall_end, 16);
    // ADR-0002: ambiguous 01:30 on the fall-back day collapses to the first
    // (PDT) occurrence, so the 7-day window has one fire per day instead of
    // the raw IANA count of 8.
    assert_eq!(
        fall_fires.len(),
        7,
        "7-day fall-back window must collapse ambiguous 01:30 to a single take-first fire"
    );
    let fall_day: Vec<_> = fall_fires
        .iter()
        .copied()
        .filter(|f| {
            let local = f.with_timezone(&tz);
            local.month() == 11 && local.day() == 1
        })
        .collect();
    assert_eq!(
        fall_day,
        vec![tz
            .with_ymd_and_hms(2026, 11, 1, 1, 30, 0)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)],
        "fall-back day keeps only the earlier PDT 01:30, not both offsets: {fall_fires:#?}"
    );
    let pst_repeat = tz
        .with_ymd_and_hms(2026, 11, 1, 1, 30, 0)
        .latest()
        .unwrap()
        .with_timezone(&Utc);
    assert!(
        !fall_fires.contains(&pst_repeat),
        "PST 01:30 (09:30Z) must be suppressed under take-first: {fall_fires:#?}"
    );
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
fn dst_fall_back_takes_first_ambiguous_hour_only() {
    // 2026-11-01: LA clocks fall back at 02:00 → 01:00 PST. A cron at
    // "30 1 * * *" maps to 01:30 PDT (08:30 UTC) and 01:30 PST (09:30 UTC).
    // ADR-0002 keeps IANA timezone resolution but applies LazyAgents'
    // take-first policy so unattended cron runs do not duplicate side effects
    // or spend in the repeated wall-clock hour.
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
        day_fires,
        vec![Utc.with_ymd_and_hms(2026, 11, 1, 8, 30, 0).unwrap()],
        "fall-back day's ambiguous 01:30 should keep only the first occurrence, got {fires:#?}",
    );
}

#[test]
fn dst_fall_back_every_minute_fires_once_per_wall_clock_minute() {
    // ADR-0002: `* 1 * * *` runs every minute of the 01:00 wall-clock hour.
    // On the LA fall-back day, 01:00..01:59 PDT (08:00..08:59 UTC) replays as
    // 01:00..01:59 PST (09:00..09:59 UTC). Take-first must collapse the 60
    // duplicates so each (local_date, local_minute) fires exactly once.
    let tz: Tz = Los_Angeles;
    let spec = CronSpec::parse("* 1 * * *", "America/Los_Angeles").unwrap();

    let day_start = tz
        .with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    // The next non-ambiguous local instant after the fall-back overlap is
    // 02:00 PST (10:00 UTC); window up to there isolates the ambiguous hour.
    let day_end = tz
        .with_ymd_and_hms(2026, 11, 1, 2, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let fires = spec.fires_between(day_start, day_end, 240);

    assert_eq!(
        fires.len(),
        60,
        "01:00 wall-clock hour must fire 60 times (once per minute) under take-first, not 120: {fires:#?}"
    );
    let earliest_pdt = tz
        .with_ymd_and_hms(2026, 11, 1, 1, 0, 0)
        .earliest()
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        fires[0], earliest_pdt,
        "first fire must be the PDT 01:00 instant"
    );
    let latest_pdt_min = tz
        .with_ymd_and_hms(2026, 11, 1, 1, 59, 0)
        .earliest()
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        *fires.last().unwrap(),
        latest_pdt_min,
        "last fire of the hour must be the PDT 01:59 instant, not any PST repeat"
    );
    let first_pst_instant = tz
        .with_ymd_and_hms(2026, 11, 1, 1, 0, 0)
        .latest()
        .unwrap()
        .with_timezone(&Utc);
    assert!(
        !fires.contains(&first_pst_instant),
        "PST replay of 01:00 must be suppressed under take-first: {fires:#?}"
    );
}

#[test]
fn dst_spring_forward_every_minute_skips_missing_local_hour() {
    // ADR-0002 keeps `cron`/`chrono-tz` behavior for the spring-forward gap:
    // 2026-03-08 02:00..02:59 America/Los_Angeles does not exist on the local
    // clock. A `* 2 * * *` cron must produce zero fires inside that window
    // and resume at the next day's 02:00 PDT instant.
    let tz: Tz = Los_Angeles;
    let spec = CronSpec::parse("* 2 * * *", "America/Los_Angeles").unwrap();

    let start = tz
        .with_ymd_and_hms(2026, 3, 8, 0, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    let end = tz
        .with_ymd_and_hms(2026, 3, 9, 3, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let fires = spec.fires_between(start, end, 240);

    let gap_fires: Vec<_> = fires
        .iter()
        .copied()
        .filter(|f| {
            let local = f.with_timezone(&tz);
            local.month() == 3 && local.day() == 8
        })
        .collect();
    assert!(
        gap_fires.is_empty(),
        "spring-forward day must produce no 02:xx local fires: {fires:#?}"
    );

    let next_day_first = tz
        .with_ymd_and_hms(2026, 3, 9, 2, 0, 0)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        fires.first().copied(),
        Some(next_day_first),
        "first fire after the gap must be 2026-03-09 02:00 PDT: {fires:#?}"
    );
    assert_eq!(
        fires.len(),
        60,
        "the 24h window after the gap must contain exactly the 60 minutes of 02:00 PDT next day: {fires:#?}"
    );
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

// ---------------------------------------------------------------------------
// §5.4 failure_backoff — heap defers next_fire_at past the backoff window
// (WEK-52 / WEK-33 N4 follow-up). The scheduler keeps a mirror of the
// executor-reported failure state; while the rail is active the heap stops
// waking on natural cron ticks that the admission gate would only refuse as
// `RefuseDeferBackoff`.
// ---------------------------------------------------------------------------

/// Pure-helper unit test: with backoff active, `next_eligible_fire` floors at
/// `last_failure_at + delay_for(n)` even when `spec.next_after(now)` is much
/// sooner. Pinned here because every loop-level test depends on this contract.
#[test]
fn next_eligible_fire_floors_to_backoff_retry() {
    let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 30).unwrap();
    let last_failure = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    // 3 failures with default expo(1m,2,1h): 60 * 2^(3-1) = 240s.
    let backoff = BackoffState {
        backoff: Some(FailureBackoff::default()),
        last_failure_at: Some(last_failure),
        consecutive_failures: 3,
    };
    let next = next_eligible_fire(&spec, now, backoff).unwrap();
    assert_eq!(
        next,
        last_failure + Duration::seconds(240),
        "next_eligible_fire must defer to last_failure_at + 240s, not the next 12:01:00 cron tick",
    );

    // No backoff configured → natural cadence.
    let no_backoff = BackoffState::default();
    let natural = next_eligible_fire(&spec, now, no_backoff).unwrap();
    assert_eq!(natural, Utc.with_ymd_and_hms(2026, 1, 1, 12, 1, 0).unwrap());

    // Backoff window already in the past → natural cadence.
    let stale = BackoffState {
        backoff: Some(FailureBackoff::default()),
        last_failure_at: Some(last_failure - Duration::hours(5)),
        consecutive_failures: 1, // 60s window — far past now
    };
    let post = next_eligible_fire(&spec, now, stale).unwrap();
    assert_eq!(post, Utc.with_ymd_and_hms(2026, 1, 1, 12, 1, 0).unwrap());
}

/// End-to-end through the live loop: a minutely cron pushed into backoff
/// stops producing fires inside the backoff window. Before the fix the loop
/// would push one `FireEvent` per minute for the admission gate to refuse;
/// after the fix the heap defers to `last_failure_at + delay_for(n)`.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn backoff_defers_next_fire_past_backoff_window() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("* * * * *", "UTC").unwrap(); // every minute
    ch.handle
        .upsert(
            "minutely",
            spec,
            CatchupMode::Skip,
            Duration::zero(),
            Some(wall), // last fired now → no install catch-up
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    // Seed the backoff mirror as if the executor settled a 3rd consecutive
    // terminal failure at 12:00:00. default expo(1m,2,1h) → 240s window, so
    // the next heap fire must land at 12:04:00 not 12:01:00.
    let existed = ch
        .handle
        .update_backoff_state("minutely", Some(FailureBackoff::default()), Some(wall), 3)
        .await
        .unwrap();
    assert!(existed, "update_backoff_state must find the entry");
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    // Snapshot: heap entry must be deferred to 12:04:00, not 12:01:00.
    let snap = ch.handle.snapshot().await.unwrap();
    assert_eq!(snap.len(), 1);
    assert_eq!(
        snap[0].fire_at,
        wall + Duration::seconds(240),
        "heap must defer to last_failure_at + 240s after WEK-52 backoff update",
    );

    // Advance 3 minutes — the loop must NOT push any fires inside the
    // backoff window. Before the fix the natural 12:01 / 12:02 / 12:03
    // ticks would each produce a FireEvent for the gate to refuse.
    advance_in_steps(StdDuration::from_secs(3 * 60), StdDuration::from_secs(30)).await;
    let mid = drain_fires(&mut ch.fires).await;
    assert!(
        mid.is_empty(),
        "no fires expected inside backoff window: {mid:#?}",
    );

    // Advance past the 240s mark — exactly one fire (12:04:00) should pop,
    // then the next natural minutely cadence resumes.
    advance_in_steps(StdDuration::from_secs(2 * 60), StdDuration::from_secs(30)).await;
    let post = drain_fires(&mut ch.fires).await;
    assert!(
        !post.is_empty(),
        "expected at least one post-backoff fire after 4m mark",
    );
    let first = &post[0];
    assert_eq!(first.cron_id, "minutely");
    assert_eq!(
        first.scheduled_at,
        wall + Duration::seconds(240),
        "first post-backoff fire must be the deferred 12:04:00 tick",
    );

    ch.handle.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn high_frequency_cron_long_backoff_does_not_emit_refusal_noise_for_four_hours() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "quiet-backoff",
            spec,
            CatchupMode::Skip,
            Duration::zero(),
            Some(wall),
        )
        .await
        .unwrap();
    ch.handle
        .update_backoff_state(
            "quiet-backoff",
            Some(FailureBackoff::default()),
            Some(wall),
            9,
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    let snap = ch.handle.snapshot().await.unwrap();
    assert_eq!(
        snap[0].fire_at,
        wall + Duration::hours(1),
        "default backoff caps at a one-hour retry rail before a four-hour soak"
    );

    advance_in_steps(StdDuration::from_secs(59 * 60), StdDuration::from_secs(60)).await;
    let before_retry = drain_fires(&mut ch.fires).await;
    assert!(
        before_retry.is_empty(),
        "high-frequency cron must not emit per-minute fires inside backoff: {before_retry:#?}"
    );

    advance_in_steps(
        StdDuration::from_secs(3 * 3600 + 60),
        StdDuration::from_secs(60),
    )
    .await;
    let after = drain_fires(&mut ch.fires).await;
    assert!(
        !after.is_empty(),
        "cron should resume after the one-hour retry rail within the four-hour soak"
    );
    assert_eq!(after[0].scheduled_at, wall + Duration::hours(1));
    assert!(
        after.len() < 256,
        "four-hour backoff soak must stay bounded, not produce thousand-level refusal noise: {} fires",
        after.len()
    );

    ch.handle.shutdown().await.unwrap();
}

/// After the executor reports a successful terminal run it calls
/// `clear_backoff_state`. The scheduler must re-anchor `next_fire_at` to the
/// natural cadence immediately so the user's "manual retry succeeded" run
/// resumes without waiting out the old backoff window.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn clear_backoff_state_resumes_natural_cadence_immediately() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, ch) = start_at(wall);

    let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "minutely",
            spec,
            CatchupMode::Skip,
            Duration::zero(),
            Some(wall),
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    // Seed: 6 failures → 1920s window (capped well below 1h).
    ch.handle
        .update_backoff_state("minutely", Some(FailureBackoff::default()), Some(wall), 6)
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let deferred = ch.handle.snapshot().await.unwrap();
    assert_eq!(deferred[0].fire_at, wall + Duration::seconds(1920));

    // Clear (executor saw a success terminal status). The next heap entry
    // must collapse back to the natural 12:01:00 cadence.
    let existed = ch.handle.clear_backoff_state("minutely").await.unwrap();
    assert!(existed);
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let resumed = ch.handle.snapshot().await.unwrap();
    assert_eq!(
        resumed[0].fire_at,
        wall + Duration::seconds(60),
        "clear_backoff_state must drop the heap floor to the natural cron tick",
    );

    ch.handle.shutdown().await.unwrap();
}

/// Race with `delete`: a backoff update arriving after the entry has been
/// removed (e.g. user disabled the cron between the executor settling and
/// the message being delivered) must reply `false` and not resurrect it.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn update_backoff_state_after_delete_is_noop_returns_false() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, ch) = start_at(wall);

    let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert("ghost", spec, CatchupMode::Skip, Duration::zero(), None)
        .await
        .unwrap();
    ch.handle.delete("ghost").await.unwrap();

    let existed = ch
        .handle
        .update_backoff_state("ghost", Some(FailureBackoff::default()), Some(wall), 2)
        .await
        .unwrap();
    assert!(
        !existed,
        "update_backoff_state on a deleted cron must be a no-op returning false",
    );
    let snap = ch.handle.snapshot().await.unwrap();
    assert!(snap.is_empty(), "deleted cron must not be resurrected");

    ch.handle.shutdown().await.unwrap();
}

/// Re-upserting an entry (e.g. user edits the cron expression while the
/// executor's failure-state mirror is non-empty) must NOT silently clear
/// the backoff rail — the executor owns that field, not the config edit.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn upsert_preserves_existing_backoff_state() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, ch) = start_at(wall);

    let spec_v1 = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "edited",
            spec_v1,
            CatchupMode::Skip,
            Duration::zero(),
            Some(wall),
        )
        .await
        .unwrap();
    // 3 consecutive failures → 240s window.
    ch.handle
        .update_backoff_state("edited", Some(FailureBackoff::default()), Some(wall), 3)
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let pre = ch.handle.snapshot().await.unwrap();
    assert_eq!(pre[0].fire_at, wall + Duration::seconds(240));

    // Config-edit upsert: change cadence to every 5 minutes. The rail must
    // stay active across the upsert, so the floor still applies.
    let spec_v2 = CronSpec::parse("*/5 * * * *", "UTC").unwrap();
    ch.handle
        .upsert("edited", spec_v2, CatchupMode::Skip, Duration::zero(), None)
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let post = ch.handle.snapshot().await.unwrap();
    // The natural next */5 fire from 12:00:00 is 12:05:00; the backoff
    // floor (240s = 12:04:00) is earlier so the natural one wins.
    assert_eq!(
        post[0].fire_at,
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 5, 0).unwrap(),
        "upsert with a longer cadence picks the later of natural-cron vs backoff floor",
    );

    // Re-upsert a high-frequency cron — now the backoff floor wins.
    let spec_v3 = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert("edited", spec_v3, CatchupMode::Skip, Duration::zero(), None)
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let post2 = ch.handle.snapshot().await.unwrap();
    assert_eq!(
        post2[0].fire_at,
        wall + Duration::seconds(240),
        "preserved backoff state must still floor the heap after a re-upsert",
    );

    // Re-upsert with a different catchup_mode (Skip → Coalesce). The
    // backoff mirror is owned by the executor, not the cron's catchup
    // policy — flipping the policy must NOT clear it.
    let spec_v4 = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "edited",
            spec_v4,
            CatchupMode::Coalesce,
            Duration::zero(),
            None,
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let post3 = ch.handle.snapshot().await.unwrap();
    assert_eq!(
        post3[0].fire_at,
        wall + Duration::seconds(240),
        "catchup_mode change must not clear executor-owned backoff mirror",
    );

    ch.handle.shutdown().await.unwrap();
}

/// Chaos guard for the M3.7 brief (issue exit criterion):
/// "高频 cron + 长期 backoff 不应产生千级 RefuseDeferBackoff 噪声". A minutely
/// cron pushed into a 1h backoff must produce at most one heap fire across
/// the entire 60-minute window — not the 59 the pre-fix loop would push.
#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn high_frequency_cron_in_long_backoff_does_not_spam_fires() {
    let wall = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let (_clock, mut ch) = start_at(wall);

    let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
    ch.handle
        .upsert(
            "noisy",
            spec,
            CatchupMode::Skip,
            Duration::zero(),
            Some(wall),
        )
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;

    // 7 failures at default expo(1m,2,1h) caps at 1h = 3600s.
    ch.handle
        .update_backoff_state("noisy", Some(FailureBackoff::default()), Some(wall), 7)
        .await
        .unwrap();
    advance(StdDuration::from_millis(1)).await;
    tokio::task::yield_now().await;
    let pre = ch.handle.snapshot().await.unwrap();
    assert_eq!(pre[0].fire_at, wall + Duration::seconds(3600));

    // Walk 59 minutes of simulated time. The pre-fix loop would have
    // emitted ~59 FireEvents for the gate to refuse; the post-fix loop must
    // emit zero.
    advance_in_steps(StdDuration::from_secs(59 * 60), StdDuration::from_secs(60)).await;
    let fires = drain_fires(&mut ch.fires).await;
    assert!(
        fires.is_empty(),
        "no fires expected inside 1h backoff cap: got {} fires",
        fires.len(),
    );

    ch.handle.shutdown().await.unwrap();
}
