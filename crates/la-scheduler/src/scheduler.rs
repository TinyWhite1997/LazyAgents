//! Scheduling loop: peek heap → sleep_until → fire → recompute → repeat.
//!
//! See module docs in `lib.rs` for the architectural picture; this file is
//! the actual `tokio::select!` loop, the clock-skew detector, and the
//! [`SchedulerHandle`] callers use to drive it.

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
use crate::heap::{CronId, EntryTable, HeapEntry};

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
        // Drain any missed fires into the event stream synchronously — these
        // come from the daemon restart path, so we want them in the channel
        // before the first natural fire.
        if let Some(last) = last_fired_at {
            if last < now {
                let missed = spec.fires_between(last, now, MISSED_FIRES_PROBE_CAP);
                if !missed.is_empty() {
                    let outcome = apply_catchup(&missed, catchup_mode, min_replay_interval)?;
                    for fire in outcome.fires {
                        let event = FireEvent {
                            cron_id: id.clone(),
                            scheduled_at: fire.scheduled_at,
                            fired_at: now,
                            coalesced_count: fire.coalesced_count,
                            catchup_degraded: outcome.degraded_to_skip,
                        };
                        if self.fire_tx.send(event).await.is_err() {
                            return Err(Error::Invariant("fire channel closed"));
                        }
                    }
                }
            }
        }
        let next = spec.next_after(now);
        let mut guard = self.table.lock().await;
        let version = guard.upsert(
            id,
            spec,
            catchup_mode,
            min_replay_interval,
            last_fired_at.or(Some(now)),
            next,
        )?;
        Ok(version)
    }

    /// Emit every entry whose `fire_at <= now`, recompute their next fire,
    /// and reinsert. Called only from the timer arm of the select.
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

            // Need to grab the spec + policy to compute the next fire.
            let (spec, mode, throttle) = {
                let guard = self.table.lock().await;
                let Some(e) = guard.get(&top.id) else {
                    // Deleted between peek and lookup; skip.
                    continue;
                };
                (e.spec.clone(), e.catchup_mode, e.min_replay_interval)
            };

            // For the natural fire we always emit one event; if multiple
            // fires are already past (e.g. we were starved by a long lock),
            // we let the next loop iteration pull them too.
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
            // Compute the *next* fire from now (not from top.fire_at): if we
            // were late, every fire strictly between top.fire_at and now is
            // covered by the policy that handled the previous starvation.
            // For the steady-state path this is identical to `after(top.fire_at)`.
            let next = spec.next_after(now);
            let _ = mode; // unused per-fire, kept in signature for future replay-burst.
            let _ = throttle;
            let mut guard = self.table.lock().await;
            guard.refresh_next_fire(&top.id, next, Some(top.fire_at));
        }
    }

    /// Re-anchor every entry's next_fire after a clock skew. Old `last_fired_at`
    /// is preserved so any in-flight catch-up still resolves correctly.
    async fn recompute_all_after_skew(&self) -> usize {
        let now = self.clock.wall_now();
        let mut guard = self.table.lock().await;
        let n = guard.entries.len();
        guard.recompute_all(now);
        n
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
