//! Scheduling loop: peek heap → sleep_until → fire → recompute → repeat.
//!
//! See module docs in `lib.rs` for the architectural picture; this file is
//! the actual `tokio::select!` loop, the clock-skew detector, and the
//! [`SchedulerHandle`] callers use to drive it.
//!
//! ## §5.3 contract unification
//!
//! The architecture spec's catch-up policy applies *uniformly* to any path
//! where the loop discovers missed wall-time fires, not just daemon restart.
//! Three paths can produce a gap between `last_fired_at` and "now":
//!
//! 1. **Daemon restart** — `install_entry` is called with `last_fired_at`
//!    persisted from the prior session. Always exercises catch-up.
//! 2. **Clock skew (laptop suspend, NTP step)** — `recompute_all_after_skew`
//!    runs after the 60-s skew tick trips; every entry whose `last_fired_at`
//!    is now in the past must replay its policy across the gap.
//! 3. **Steady-state starvation** — `fire_due_entries` is woken late because
//!    some other entry held the lock, or the OS scheduler ran us behind.
//!    Anything strictly between `top.fire_at` and `now` is "missed" and
//!    must be processed by the entry's policy, not silently dropped.
//!
//! All three funnel through [`Scheduler::process_missed_fires`] so the same
//! `apply_catchup` resolver decides skip/coalesce/replay and the same
//! `catchup_degraded` flag fires when `MAX_CATCHUP` is breached.

use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep_until, Duration as StdDuration, Instant};
use tracing::{debug, info, warn};

use crate::catchup::{apply_catchup, CatchupMode, MAX_CATCHUP};
use crate::clock::{wall_to_instant, Clock, SharedClock};
use crate::command::Command;
use crate::cron_spec::CronSpec;
use crate::error::Error;
use crate::event::{FireEvent, SchedulerEvent};
use crate::heap::{next_eligible_fire, BackoffState, CronId, EntryTable, HeapEntry};
use crate::quota::backoff::FailureBackoff;

/// Cadence at which the loop polls for clock skew (§5.2: "每 60 s").
const SKEW_TICK: StdDuration = StdDuration::from_secs(60);
/// Skew threshold that triggers a full re-heap (§5.2: "> 30 s").
const SKEW_THRESHOLD_SECS: i64 = 30;
/// Hard ceiling on per-recovery catch-up enumeration, matching
/// [`crate::catchup::MAX_CATCHUP`]. We pass `MAX_CATCHUP + 1` to
/// `fires_between` so the resolver can *see* the overflow case.
const MISSED_FIRES_PROBE_CAP: usize = MAX_CATCHUP + 1;

/// Cap on the buffered fire event channel. 256 is comfortable for the
/// "thousands of crons / day" volume the architecture targets without
/// soaking memory.
const FIRE_CHANNEL_CAP: usize = 256;
const DIAG_CHANNEL_CAP: usize = 64;
const COMMAND_CHANNEL_CAP: usize = 64;

/// Caller-facing handle for sending commands. Cheap to clone.
#[derive(Clone)]
pub struct SchedulerHandle {
    tx: mpsc::Sender<Command>,
}

impl SchedulerHandle {
    /// Insert or replace a cron. `last_fired_at` lets a daemon restart
    /// resume catch-up from the persisted high-water mark.
    pub async fn upsert(
        &self,
        id: impl Into<CronId>,
        spec: CronSpec,
        catchup_mode: CatchupMode,
        min_replay_interval: ChronoDuration,
        last_fired_at: Option<DateTime<Utc>>,
    ) -> Result<u64, Error> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Command::Upsert {
                id: id.into(),
                spec: Box::new(spec),
                catchup_mode,
                min_replay_interval,
                last_fired_at,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Invariant("scheduler loop closed"))?;
        reply_rx
            .await
            .map_err(|_| Error::Invariant("scheduler reply dropped"))?
    }

    /// Remove a cron. Returns `true` if it existed.
    pub async fn delete(&self, id: impl Into<CronId>) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Command::Delete {
                id: id.into(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Invariant("scheduler loop closed"))?;
        reply_rx
            .await
            .map_err(|_| Error::Invariant("scheduler reply dropped"))
    }

    /// Snapshot of upcoming fires, ordered earliest first. Used by the IPC
    /// `crons.list` / status-bar surfaces.
    pub async fn snapshot(&self) -> Result<Vec<HeapEntry>, Error> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Command::Snapshot { reply: reply_tx })
            .await
            .map_err(|_| Error::Invariant("scheduler loop closed"))?;
        reply_rx
            .await
            .map_err(|_| Error::Invariant("scheduler reply dropped"))
    }

    /// Best-effort wake of the loop. Used by tests; production callers do
    /// not need this because `upsert` / `delete` already wake.
    pub async fn poke(&self) -> Result<(), Error> {
        self.tx
            .send(Command::Poke)
            .await
            .map_err(|_| Error::Invariant("scheduler loop closed"))?;
        Ok(())
    }

    /// Stop the loop. Subsequent commands fail with `Invariant("…closed")`.
    pub async fn shutdown(&self) -> Result<(), Error> {
        let _ = self.tx.send(Command::Shutdown).await;
        Ok(())
    }

    /// Mirror the daemon executor's per-cron failure-backoff state into the
    /// scheduler so the heap floors `next_fire_at` at
    /// `last_failure_at + delay_for(consecutive_failures)`. Returns `true`
    /// when the entry existed.
    ///
    /// ## Executor contract (WEK-52 / §5.4 "连续失败时延后下一次触发")
    ///
    /// The daemon's run executor is the authority on the three inputs
    /// (`failure_backoff` parsed from the `crons` row,
    /// `crons.consecutive_failures` counter, `runs.finished_at` of the
    /// most recent terminal failure). The scheduler keeps a mirror so the
    /// heap can defer the next wake-up without re-querying SQLite. The
    /// contract is:
    ///
    /// - **After every terminal run** (status `failed` / `timed_out` /
    ///   `completed` / `cancelled` / `budget_exceeded`), once the executor
    ///   has settled the new counter, call this with the
    ///   post-update values:
    ///     * Terminal failure → `consecutive_failures = N + 1`,
    ///       `last_failure_at = Some(now)`, `backoff = parsed or DDL default`.
    ///     * Successful or non-failure terminal → call
    ///       [`Self::clear_backoff_state`] (same as passing
    ///       `consecutive_failures = 0`).
    /// - **After a config edit** that changes the parsed `failure_backoff`
    ///   string, call this with the new `backoff` and the existing counter /
    ///   timestamp so the rail reflects the user's edit immediately. A
    ///   normal `upsert` preserves the existing mirror so the executor does
    ///   not have to re-push after every TUI tweak.
    /// - **Race with `delete`**: if the entry has already been removed the
    ///   call is a no-op and replies `false`.
    pub async fn update_backoff_state(
        &self,
        id: impl Into<CronId>,
        backoff: Option<FailureBackoff>,
        last_failure_at: Option<DateTime<Utc>>,
        consecutive_failures: u32,
    ) -> Result<bool, Error> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Command::UpdateBackoffState {
                id: id.into(),
                backoff,
                last_failure_at,
                consecutive_failures,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Invariant("scheduler loop closed"))?;
        reply_rx
            .await
            .map_err(|_| Error::Invariant("scheduler reply dropped"))
    }

    /// Convenience for the executor's success path: clear the backoff rail
    /// for `id` (zero counter, no recorded failure). Equivalent to
    /// `update_backoff_state(id, None, None, 0)`.
    pub async fn clear_backoff_state(&self, id: impl Into<CronId>) -> Result<bool, Error> {
        self.update_backoff_state(id, None, None, 0).await
    }
}

/// All the channels the scheduler returns from [`Scheduler::start`].
pub struct SchedulerChannels {
    pub handle: SchedulerHandle,
    pub fires: mpsc::Receiver<FireEvent>,
    pub diagnostics: mpsc::Receiver<SchedulerEvent>,
}

/// The scheduler runtime. Construct with [`Scheduler::start`] and drive the
/// returned channels.
pub struct Scheduler {
    clock: SharedClock,
    table: Arc<Mutex<EntryTable>>,
    cmd_rx: mpsc::Receiver<Command>,
    fire_tx: mpsc::Sender<FireEvent>,
    diag_tx: mpsc::Sender<SchedulerEvent>,
}

/// Inputs to [`Scheduler::process_missed_fires`]. Packed into a struct so the
/// three caller sites (install / skew-recompute / fire-starvation) share one
/// signature and the code-review diff is trivial when we add a field.
struct MissedFireInputs<'a> {
    id: &'a str,
    spec: &'a CronSpec,
    mode: CatchupMode,
    min_replay_interval: ChronoDuration,
    /// The high-water mark to walk forward from (exclusive). Catch-up
    /// enumerates `(last_fired_at, end]`.
    last_fired_at: DateTime<Utc>,
    /// The "now" boundary fires are walked to (inclusive). Always wall time.
    end: DateTime<Utc>,
    /// Wall time the fires are being emitted on (becomes `FireEvent.fired_at`).
    fired_at: DateTime<Utc>,
}

/// Snapshot row taken under the heap lock by `recompute_all_after_skew` so
/// the per-entry catch-up loop can run without holding the lock. Named
/// instead of a tuple to keep `Vec<…>` readable and silence clippy's
/// `type_complexity` lint.
struct SkewWorkItem {
    id: CronId,
    spec: CronSpec,
    mode: CatchupMode,
    throttle: ChronoDuration,
    last_fired: Option<DateTime<Utc>>,
}

impl Scheduler {
    /// Start a scheduler bound to `clock`, returning the control handle and
    /// the fire / diagnostics streams. The actual loop runs as a Tokio task;
    /// the returned `JoinHandle` lets callers await graceful shutdown.
    pub fn start(clock: SharedClock) -> (SchedulerChannels, tokio::task::JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAP);
        let (fire_tx, fire_rx) = mpsc::channel(FIRE_CHANNEL_CAP);
        let (diag_tx, diag_rx) = mpsc::channel(DIAG_CHANNEL_CAP);
        let scheduler = Scheduler {
            clock,
            table: Arc::new(Mutex::new(EntryTable::new())),
            cmd_rx,
            fire_tx,
            diag_tx,
        };
        let join = tokio::spawn(async move { scheduler.run().await });
        (
            SchedulerChannels {
                handle: SchedulerHandle { tx: cmd_tx },
                fires: fire_rx,
                diagnostics: diag_rx,
            },
            join,
        )
    }

    async fn run(mut self) {
        info!("la-scheduler loop starting");
        // Anchor for clock-skew detection. We re-anchor every successful tick
        // so cumulative drift is measured against the most recent baseline.
        let mut skew_anchor = SkewAnchor::sample(&*self.clock);
        let mut skew_tick = tokio::time::interval_at(Instant::now() + SKEW_TICK, SKEW_TICK);
        // First tick is consumed immediately by `interval_at` semantics; that's
        // fine — we only act after the second tick fires.

        loop {
            // Peek the soonest fire under the lock, then drop the guard
            // before sleeping so commands can race in.
            let next: Option<HeapEntry> = {
                let mut guard = self.table.lock().await;
                guard.peek_next()
            };

            let sleep_deadline = match &next {
                Some(entry) => wall_to_instant(&*self.clock, entry.fire_at),
                // No entries: park ~1 hour but allow commands / skew tick to
                // wake us. Using a huge sleep instead of `pending()` keeps
                // the loop's structure uniform.
                None => self.clock.mono_now() + StdDuration::from_secs(3600),
            };

            tokio::select! {
                biased; // commands should win against fire / tick in the same iteration

                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(Command::Shutdown) | None => {
                            info!("la-scheduler shutting down");
                            return;
                        }
                        Some(c) => self.apply_command(c).await,
                    }
                }

                _ = skew_tick.tick() => {
                    if let Some(skew) = skew_anchor.check(&*self.clock) {
                        warn!(skew_seconds = skew, "clock skew detected; recomputing schedule");
                        let recomputed = self.recompute_all_after_skew().await;
                        let _ = self
                            .diag_tx
                            .try_send(SchedulerEvent::ClockSkewDetected {
                                skew_seconds: skew,
                                recomputed_entries: recomputed,
                            });
                        skew_anchor = SkewAnchor::sample(&*self.clock);
                    }
                }

                _ = sleep_until(sleep_deadline), if next.is_some() => {
                    // Even though we woke for `next`, re-peek under the lock —
                    // a command may have invalidated it while we slept.
                    let live = {
                        let mut guard = self.table.lock().await;
                        guard.peek_next()
                    };
                    if let Some(entry) = live {
                        // Only act if the entry we slept for is still the
                        // earliest *and* its fire_at is in the past (relative
                        // to wall_now); otherwise loop back and re-sleep.
                        let now_wall = self.clock.wall_now();
                        if entry.fire_at <= now_wall {
                            self.fire_due_entries(now_wall).await;
                        }
                    }
                }
            }
        }
    }

    async fn apply_command(&self, cmd: Command) {
        match cmd {
            Command::Upsert {
                id,
                spec,
                catchup_mode,
                min_replay_interval,
                last_fired_at,
                reply,
            } => {
                let res = self
                    .install_entry(id, *spec, catchup_mode, min_replay_interval, last_fired_at)
                    .await;
                let _ = reply.send(res);
            }
            Command::Delete { id, reply } => {
                let existed = {
                    let mut guard = self.table.lock().await;
                    guard.delete(&id).is_some()
                };
                let _ = reply.send(existed);
            }
            Command::Poke => { /* loop wakes; nothing else to do */ }
            Command::Snapshot { reply } => {
                let snap = {
                    let guard = self.table.lock().await;
                    let mut out = Vec::with_capacity(guard.entries.len());
                    // We need to walk through the entries map, not the heap —
                    // the heap may contain stale entries we'd otherwise hide.
                    for entry in guard.entries.values() {
                        if let Some(fire) = entry.next_fire_at {
                            out.push(HeapEntry {
                                fire_at: fire,
                                id: entry.id.clone(),
                                version: entry.version,
                            });
                        }
                    }
                    out.sort();
                    out
                };
                let _ = reply.send(snap);
            }
            Command::UpdateBackoffState {
                id,
                backoff,
                last_failure_at,
                consecutive_failures,
                reply,
            } => {
                let new_state = BackoffState {
                    backoff,
                    last_failure_at,
                    consecutive_failures,
                };
                let existed = self.apply_backoff_update(&id, new_state).await;
                let _ = reply.send(existed);
            }
            Command::Shutdown => unreachable!("handled in select arm"),
        }
    }

    /// Compute next_fire_at for a fresh upsert, applying any catch-up the
    /// caller's `last_fired_at` requires, and insert into the heap.
    async fn install_entry(
        &self,
        id: CronId,
        spec: CronSpec,
        catchup_mode: CatchupMode,
        min_replay_interval: ChronoDuration,
        last_fired_at: Option<DateTime<Utc>>,
    ) -> Result<u64, Error> {
        let now = self.clock.wall_now();
        // Run the §5.3 catch-up and, crucially, advance the persisted
        // watermark to `now` so a subsequent skew-recompute or starvation
        // pass cannot rewalk the same gap and double-fire. (The first
        // version of this code stored the raw `last_fired_at` we were
        // handed, which let `recompute_all_after_skew` replay every fire
        // install had just emitted — see the
        // `install_then_skew_does_not_double_fire_catchup` regression test.)
        let watermark_after_catchup = match last_fired_at {
            Some(last) if last < now => {
                self.process_missed_fires(MissedFireInputs {
                    id: &id,
                    spec: &spec,
                    mode: catchup_mode,
                    min_replay_interval,
                    last_fired_at: last,
                    end: now,
                    fired_at: now,
                })
                .await?;
                Some(now)
            }
            other => other,
        };
        // Preserve any backoff mirror the executor may have already pushed
        // for this id (e.g. a config-edit upsert that follows a failure
        // observation). `EntryTable::upsert` carries the old mirror forward;
        // we read it back so the heap's first `next_fire_at` reflects it.
        let preserved_backoff = {
            let guard = self.table.lock().await;
            guard.get(&id).map(|e| e.backoff).unwrap_or_default()
        };
        let next = next_eligible_fire(&spec, now, preserved_backoff);
        let mut guard = self.table.lock().await;
        let version = guard.upsert(
            id,
            spec,
            catchup_mode,
            min_replay_interval,
            watermark_after_catchup.or(Some(now)),
            next,
        )?;
        Ok(version)
    }

    /// Apply a backoff-state mirror update from the daemon executor and
    /// re-anchor the entry's `next_fire_at` so the heap reflects the new
    /// deferral immediately. Returns `true` when the entry existed.
    ///
    /// We re-walk the spec from `now` (not from `last_fired_at`) and floor
    /// the result at the backoff retry instant. That matches the contract
    /// the gate enforces — the rail defers the *next* fire, not a missed
    /// historic one. Catch-up policy is untouched: a `coalesce` user that
    /// suspended for hours during a backoff still sees the merged fire from
    /// the skew path, because that path runs `process_missed_fires` against
    /// the watermark *before* re-anchoring through `next_eligible_fire`.
    async fn apply_backoff_update(&self, id: &CronId, new_state: BackoffState) -> bool {
        let mut guard = self.table.lock().await;
        if guard.set_backoff(id, new_state).is_none() {
            return false;
        }
        // `get(&id)` is guaranteed Some — we just wrote it.
        let spec = guard
            .get(id)
            .map(|e| e.spec.clone())
            .expect("entry present after set_backoff");
        let now = self.clock.wall_now();
        let next = next_eligible_fire(&spec, now, new_state);
        guard.refresh_next_fire(id, next, None);
        true
    }

    /// Emit every entry whose `fire_at <= now`. If the loop was starved (so
    /// `now > top.fire_at` by more than one tick), the gap between
    /// `entry.last_fired_at` (or `top.fire_at`) and `now` is handed to the
    /// per-entry catch-up policy before recomputing the next fire — the
    /// "steady-state starvation" path from the module docs.
    async fn fire_due_entries(&self, now: DateTime<Utc>) {
        loop {
            // Pop one due entry at a time so each fire becomes its own event
            // and contention on the lock stays short.
            let popped = {
                let mut guard = self.table.lock().await;
                let Some(top) = guard.peek_next() else {
                    return;
                };
                if top.fire_at > now {
                    return;
                }
                guard.pop_next()
            };
            let Some(top) = popped else { return };

            // Snapshot the policy + spec + watermark under the lock; we drop
            // the guard before pushing events so the loop stays responsive.
            let (spec, mode, throttle, last_fired_at, backoff) = {
                let guard = self.table.lock().await;
                let Some(e) = guard.get(&top.id) else {
                    // Deleted between peek and lookup; skip.
                    continue;
                };
                (
                    e.spec.clone(),
                    e.catchup_mode,
                    e.min_replay_interval,
                    e.last_fired_at,
                    e.backoff,
                )
            };

            // Always emit the fire we popped. `coalesced_count = 1` because
            // any merging happens inside `process_missed_fires` over the
            // *gap*, not on the top entry itself.
            let event = FireEvent {
                cron_id: top.id.clone(),
                scheduled_at: top.fire_at,
                fired_at: now,
                coalesced_count: 1,
                catchup_degraded: false,
            };
            if self.fire_tx.send(event).await.is_err() {
                warn!("fire channel closed; dropping further events");
                return;
            }

            // Starvation gap: process anything strictly between the entry we
            // just fired and `now`. The watermark we walk forward from is
            // `top.fire_at` itself, NOT `last_fired_at`, because `top` has
            // now been emitted — `fires_between` is exclusive on the lower
            // bound so this naturally skips it.
            //
            // For the steady-state path (popped entry's fire_at == now)
            // `fires_between(top.fire_at, now)` is empty and this is free.
            let _ = last_fired_at; // future use: cross-check with persisted high-water.
            if top.fire_at < now {
                if let Err(e) = self
                    .process_missed_fires(MissedFireInputs {
                        id: &top.id,
                        spec: &spec,
                        mode,
                        min_replay_interval: throttle,
                        last_fired_at: top.fire_at,
                        end: now,
                        fired_at: now,
                    })
                    .await
                {
                    warn!(error = %e, cron_id = %top.id, "starvation catch-up failed");
                    return;
                }
            }

            // Next natural fire is computed from `now`: anything between
            // `top.fire_at` and `now` has already been resolved above, so
            // walking from `top.fire_at` would replay them again. For the
            // same reason the persisted watermark is advanced to `now` when
            // catch-up ran — leaving it at `top.fire_at` would let a
            // subsequent skew-recompute rewalk the same gap and double-fire
            // every emission this starvation pass just produced.
            //
            // We pass through `next_eligible_fire` so an active backoff
            // floors `next_fire_at` at `last_failure_at + delay_for(n)` —
            // without that, a high-frequency cron in a long backoff window
            // (e.g. every-minute cron in expo(1m,2,1h) after 6 failures)
            // would still wake the loop on every cron tick just to push an
            // event that the admission gate refuses as `RefuseDeferBackoff`
            // (WEK-52).
            let next = next_eligible_fire(&spec, now, backoff);
            let new_last = if top.fire_at < now { now } else { top.fire_at };
            let mut guard = self.table.lock().await;
            guard.refresh_next_fire(&top.id, next, Some(new_last));
        }
    }

    /// After a clock-skew trip, re-anchor every entry's `next_fire_at` AND
    /// run each entry's catch-up policy across the skew gap. Without the
    /// second step, `coalesce` / `replay` users would silently lose their
    /// missed fires whenever a laptop wakes from suspend — exactly the
    /// scenario §5.2's skew detector is designed to *handle*, not just
    /// detect. Returns the number of entries that were re-anchored.
    async fn recompute_all_after_skew(&self) -> usize {
        let now = self.clock.wall_now();

        // Snapshot the catch-up inputs under the lock, then release it so we
        // can push events without contending with the main loop.
        let work: Vec<SkewWorkItem> = {
            let guard = self.table.lock().await;
            guard
                .entries
                .values()
                .map(|e| SkewWorkItem {
                    id: e.id.clone(),
                    spec: e.spec.clone(),
                    mode: e.catchup_mode,
                    throttle: e.min_replay_interval,
                    last_fired: e.last_fired_at,
                })
                .collect()
        };
        let count = work.len();

        for item in &work {
            if let Some(last) = item.last_fired {
                if last < now {
                    if let Err(e) = self
                        .process_missed_fires(MissedFireInputs {
                            id: &item.id,
                            spec: &item.spec,
                            mode: item.mode,
                            min_replay_interval: item.throttle,
                            last_fired_at: last,
                            end: now,
                            fired_at: now,
                        })
                        .await
                    {
                        warn!(error = %e, cron_id = %item.id, "skew catch-up failed");
                    }
                }
            }
        }

        // Now re-anchor next_fire_at from `now` for every entry — same as
        // before, but it runs *after* catch-up so the heap reflects the
        // post-gap state. `last_fired_at` is bumped to `now` for any entry
        // we just emitted catch-up fires for, so the next starvation pass
        // doesn't replay them. We route through `next_eligible_fire` so an
        // active backoff still defers the post-skew wake-up (a coalesce/
        // replay cron that catches up after a suspend should still respect
        // the executor-reported retry window — WEK-52).
        let mut guard = self.table.lock().await;
        let now_after = self.clock.wall_now();
        for item in work {
            let next = guard
                .entries
                .get(&item.id)
                .and_then(|e| next_eligible_fire(&e.spec, now_after, e.backoff));
            // If we emitted catch-up fires for this entry, advance its
            // last_fired_at watermark so subsequent ticks don't double-fire.
            let new_last = match item.last_fired {
                Some(l) if l < now_after => Some(now_after),
                _ => None, // refresh_next_fire(None) leaves last_fired_at untouched.
            };
            guard.refresh_next_fire(&item.id, next, new_last);
        }

        count
    }

    /// Shared catch-up emitter (§5.3). Walks the spec for missed fires in
    /// `(inputs.last_fired_at, inputs.end]`, runs the resolver, and pushes
    /// one `FireEvent` per resolved emission onto the fire channel.
    ///
    /// Returns `Err(Error::Invariant)` if the fire channel has been closed
    /// by the consumer — the loop treats that as a fatal exit signal.
    async fn process_missed_fires(&self, inputs: MissedFireInputs<'_>) -> Result<(), Error> {
        let missed =
            inputs
                .spec
                .fires_between(inputs.last_fired_at, inputs.end, MISSED_FIRES_PROBE_CAP);
        if missed.is_empty() {
            return Ok(());
        }
        let outcome = apply_catchup(&missed, inputs.mode, inputs.min_replay_interval)?;
        for fire in outcome.fires {
            let event = FireEvent {
                cron_id: inputs.id.to_string(),
                scheduled_at: fire.scheduled_at,
                fired_at: inputs.fired_at,
                coalesced_count: fire.coalesced_count,
                catchup_degraded: outcome.degraded_to_skip,
            };
            if self.fire_tx.send(event).await.is_err() {
                return Err(Error::Invariant("fire channel closed"));
            }
        }
        Ok(())
    }
}

/// Tracks wall vs monotonic drift across the 60-s skew tick (§5.2).
struct SkewAnchor {
    wall: DateTime<Utc>,
    mono: Instant,
}

impl SkewAnchor {
    fn sample(clock: &dyn Clock) -> Self {
        Self {
            wall: clock.wall_now(),
            mono: clock.mono_now(),
        }
    }

    /// If `|wall_delta - mono_delta| > SKEW_THRESHOLD_SECS`, return the signed
    /// skew in seconds. Otherwise `None`.
    fn check(&self, clock: &dyn Clock) -> Option<i64> {
        let now_wall = clock.wall_now();
        let now_mono = clock.mono_now();
        let wall_delta_secs = (now_wall - self.wall).num_seconds();
        let mono_delta_std = now_mono.saturating_duration_since(self.mono);
        let mono_delta_secs = mono_delta_std.as_secs() as i64;
        let skew = wall_delta_secs - mono_delta_secs;
        if skew.abs() > SKEW_THRESHOLD_SECS {
            debug!(wall_delta_secs, mono_delta_secs, skew, "skew check tripped");
            Some(skew)
        } else {
            None
        }
    }
}
