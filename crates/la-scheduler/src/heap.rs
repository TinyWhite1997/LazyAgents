//! In-memory entry table + min-heap keyed by next-fire wall time.
//!
//! Two cooperating structures:
//! - `entries: HashMap<CronId, Entry>` — authoritative state.
//! - `heap: BinaryHeap<Reverse<HeapEntry>>` — what the scheduler loop peeks
//!   to know "what fires soonest".
//!
//! Heap entries are tagged with a monotonically incremented `version`. When
//! `upsert`/`delete` rewrites or removes an entry, we bump its `version` in
//! `entries` and leave the stale heap entry behind — when the loop pops it,
//! the version check rejects it as obsolete. This is the standard "lazy
//! deletion" pattern for `BinaryHeap`, which has no `decrease_key`.
//!
//! The architecture (verification standard for §5.2) demands "heap 重排在
//! upsert / delete 后即时生效" — that's why every mutating call bumps the
//! entry's version, and the scheduler loop wakes on the command-channel
//! `mpsc` (in [`crate::scheduler::Scheduler::run`]) the moment a change lands.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use chrono::{DateTime, Utc};

use crate::catchup::CatchupMode;
use crate::cron_spec::CronSpec;
use crate::quota::backoff::FailureBackoff;
use crate::Error;

/// Stable, caller-supplied id for a scheduled cron. The scheduler doesn't
/// look inside — it's just an opaque key. We type-alias `String` to keep the
/// public surface explicit.
pub type CronId = String;

/// Per-cron mutable state held in the heap table.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: CronId,
    pub spec: CronSpec,
    pub catchup_mode: CatchupMode,
    /// Throttle for `replay` mode (`min_interval_s` in §5.3). Zero means no
    /// throttle.
    pub min_replay_interval: chrono::Duration,
    /// Last *wall-time* the cron actually fired, used by catch-up on
    /// recovery. `None` for never-fired entries (first run uses now).
    pub last_fired_at: Option<DateTime<Utc>>,
    /// Cached next fire (in UTC). Always derivable from `spec` + `last`, but
    /// we materialise it for fast peeking and event emission.
    pub next_fire_at: Option<DateTime<Utc>>,
    /// Failure-backoff state mirrored from the daemon's run executor.
    /// When `consecutive_failures > 0` and `failure_backoff` is `Some(_)`,
    /// the scheduler floors `next_fire_at` at
    /// `last_terminal_failure_at + backoff.delay_for(consecutive_failures)`,
    /// so a high-frequency cron in backoff does not waste scheduler wake-ups
    /// firing into an admission gate that will only return
    /// `RefuseDeferBackoff` (WEK-52 / WEK-33 N4).
    pub backoff: BackoffState,
    /// Monotonically-incremented marker that lets us spot stale heap entries
    /// after upsert/delete. Bumped on every state change that affects
    /// `next_fire_at` or removes the entry.
    pub version: u64,
}

/// Per-cron failure-backoff state mirrored from the daemon's run executor.
///
/// The daemon owns the authoritative copy in SQLite (`crons.consecutive_failures`,
/// `runs.finished_at` of the most recent terminal failure, parsed
/// `crons.failure_backoff`); this struct is the scheduler's local mirror,
/// updated via [`crate::SchedulerHandle::update_backoff_state`] every time
/// the executor settles a terminal run.
///
/// All-zero / all-`None` is the "no active backoff" sentinel, matching the
/// case after a successful run (executor calls
/// [`crate::SchedulerHandle::clear_backoff_state`] to reset). When the rail
/// is inactive the helper [`next_eligible_fire`] returns `None` and the
/// scheduler falls back to the natural `spec.next_after(now)` cadence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackoffState {
    pub backoff: Option<FailureBackoff>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
}

impl BackoffState {
    /// Earliest wall instant at which the backoff rail would admit again,
    /// or `None` when the rail is inactive (no backoff configured, no prior
    /// failure, or `consecutive_failures == 0`). Mirrors
    /// [`crate::quota::backoff_retry_after`] without taking the full quota
    /// struct — the scheduler only needs the three fields tracked here.
    ///
    /// **All-or-nothing semantics (fail-open).** The three inputs
    /// (`backoff`, `last_failure_at`, `consecutive_failures`) are treated
    /// as one bundle: missing any single piece (e.g. `Some(backoff) +
    /// consecutive_failures > 0 + last_failure_at = None`) returns `None`
    /// rather than synthesising a window from defaults. The choice is
    /// deliberate — if the executor failed to thread the timestamp through,
    /// we would rather temporarily lose the deferral (the admission gate
    /// is the safety net) than wedge the heap into a permanent
    /// retry-after-Y2K window.
    pub fn retry_after(self) -> Option<DateTime<Utc>> {
        let backoff = self.backoff?;
        let last = self.last_failure_at?;
        if self.consecutive_failures == 0 {
            return None;
        }
        let delay = backoff.delay_for(self.consecutive_failures);
        if delay.is_zero() {
            return None;
        }
        let delta = chrono::Duration::from_std(delay).ok()?;
        last.checked_add_signed(delta)
    }
}

/// Compute the scheduler's next-fire instant from `spec.next_after(now)`,
/// floored at the backoff rail's `retry_after` when one is active. Returns
/// `None` only when the cron spec itself has no future fire.
///
/// This is the single point §5.4 "连续失败时延后下一次触发" gets enforced
/// inside la-scheduler: every place that would have called
/// `spec.next_after(now)` directly now goes through here so the heap reflects
/// the deferral instead of waking on every cron tick just to be refused at
/// the admission gate.
pub fn next_eligible_fire(
    spec: &CronSpec,
    now: DateTime<Utc>,
    backoff: BackoffState,
) -> Option<DateTime<Utc>> {
    let natural = spec.next_after(now)?;
    match backoff.retry_after() {
        Some(retry) if retry > natural => Some(retry),
        _ => Some(natural),
    }
}

/// What the heap actually stores. We pull `next_fire_at` and `version` out
/// onto the heap entry so the loop can peek/pop without going through the
/// HashMap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapEntry {
    pub fire_at: DateTime<Utc>,
    pub id: CronId,
    pub version: u64,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Order by fire_at, tie-break on id so two crons firing at the same
        // instant pop in a deterministic order — tests rely on this.
        self.fire_at
            .cmp(&other.fire_at)
            .then_with(|| self.id.cmp(&other.id))
            .then_with(|| self.version.cmp(&other.version))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The heap + entry table. Not `Send`-shared on its own; the scheduler wraps
/// it in `Arc<Mutex<…>>` and exposes only narrow mutating helpers.
#[derive(Debug, Default)]
pub struct EntryTable {
    pub(crate) entries: HashMap<CronId, Entry>,
    pub(crate) heap: BinaryHeap<Reverse<HeapEntry>>,
}

impl EntryTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of *live* (non-deleted) entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or replace an entry, bumping its version and pushing a fresh
    /// heap node so the scheduler will see the new fire time on its next
    /// peek. Returns the new version.
    ///
    /// `next_fire_at` may be precomputed by the caller (handy after catch-up
    /// resolution) or recomputed via `spec.next_after(now)`. Either way, this
    /// method only stores what it's given — it never silently re-derives a
    /// different value.
    pub fn upsert(
        &mut self,
        id: CronId,
        spec: CronSpec,
        catchup_mode: CatchupMode,
        min_replay_interval: chrono::Duration,
        last_fired_at: Option<DateTime<Utc>>,
        next_fire_at: Option<DateTime<Utc>>,
    ) -> Result<u64, Error> {
        // Preserve any existing backoff mirror across re-upserts: a config
        // edit (changing the cron expression / catchup mode) should not
        // silently clear the executor-reported failure state, which only
        // the executor itself is authoritative over.
        let preserved_backoff = self
            .entries
            .get(&id)
            .map(|e| e.backoff)
            .unwrap_or_default();
        let version = self.entries.get(&id).map(|e| e.version + 1).unwrap_or(1);
        let entry = Entry {
            id: id.clone(),
            spec,
            catchup_mode,
            min_replay_interval,
            last_fired_at,
            next_fire_at,
            backoff: preserved_backoff,
            version,
        };
        if let Some(fire) = next_fire_at {
            self.heap.push(Reverse(HeapEntry {
                fire_at: fire,
                id: id.clone(),
                version,
            }));
        }
        self.entries.insert(id, entry);
        Ok(version)
    }

    /// Remove an entry. Old heap nodes stay until the loop pops them, at
    /// which point the missing entry / version mismatch causes them to be
    /// discarded.
    pub fn delete(&mut self, id: &str) -> Option<Entry> {
        self.entries.remove(id)
    }

    /// Look up an entry by id. Read-only.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }

    /// Earliest fire time in the heap, *after* skipping stale entries.
    /// Returns the live `HeapEntry`; callers should consult the entry table
    /// (e.g. for the spec) by id.
    pub fn peek_next(&mut self) -> Option<HeapEntry> {
        loop {
            let top = self.heap.peek()?.0.clone();
            match self.entries.get(&top.id) {
                Some(entry) if entry.version == top.version => return Some(top),
                _ => {
                    // Stale: corresponding entry was deleted or rewritten.
                    self.heap.pop();
                }
            }
        }
    }

    /// Pop the earliest fire time IF it matches the live entry. Returns
    /// `None` if the heap is empty (after stale cleanup).
    pub fn pop_next(&mut self) -> Option<HeapEntry> {
        loop {
            let top = self.heap.pop()?.0;
            match self.entries.get(&top.id) {
                Some(entry) if entry.version == top.version => return Some(top),
                _ => continue,
            }
        }
    }

    /// Rewrite `next_fire_at` on an existing entry without changing its
    /// spec — used by the scheduler loop after a fire, and by the clock-skew
    /// detector when it recomputes every entry. Returns the new version, or
    /// `None` if the entry has been deleted.
    pub fn refresh_next_fire(
        &mut self,
        id: &str,
        next_fire_at: Option<DateTime<Utc>>,
        last_fired_at: Option<DateTime<Utc>>,
    ) -> Option<u64> {
        let entry = self.entries.get_mut(id)?;
        entry.version += 1;
        entry.next_fire_at = next_fire_at;
        if let Some(lf) = last_fired_at {
            entry.last_fired_at = Some(lf);
        }
        let version = entry.version;
        if let Some(fire) = next_fire_at {
            self.heap.push(Reverse(HeapEntry {
                fire_at: fire,
                id: id.to_string(),
                version,
            }));
        }
        Some(version)
    }

    /// Overwrite the executor-reported backoff mirror without touching the
    /// spec / catchup mode / next-fire fields. Used by
    /// [`crate::SchedulerHandle::update_backoff_state`] and
    /// [`crate::SchedulerHandle::clear_backoff_state`]. Returns the previous
    /// backoff value when the entry exists, or `None` when it has been
    /// deleted (in which case the caller has nothing to update).
    ///
    /// Does NOT bump the version or push a heap entry — the scheduler loop
    /// re-pushes via [`Self::refresh_next_fire`] right after, with the
    /// backoff-aware `next_fire_at` already folded in.
    pub fn set_backoff(&mut self, id: &str, backoff: BackoffState) -> Option<BackoffState> {
        let entry = self.entries.get_mut(id)?;
        let prev = entry.backoff;
        entry.backoff = backoff;
        Some(prev)
    }

    /// Iterator over live entries; the order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// Reset every entry's `next_fire_at` by walking the spec from `now`,
    /// honouring the per-entry `backoff` mirror so a deferred cron stays
    /// deferred across the re-anchor. Past `last_fired_at` values are kept
    /// so a downstream recovery pass can still catch up missed fires.
    pub fn recompute_all(&mut self, now: DateTime<Utc>) {
        let ids: Vec<_> = self.entries.keys().cloned().collect();
        for id in ids {
            let next = self
                .entries
                .get(&id)
                .and_then(|e| next_eligible_fire(&e.spec, now, e.backoff));
            self.refresh_next_fire(&id, next, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron_spec::CronSpec;
    use chrono::TimeZone;

    fn spec(expr: &str) -> CronSpec {
        CronSpec::parse(expr, "UTC").unwrap()
    }

    #[test]
    fn upsert_and_peek() {
        let mut t = EntryTable::new();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let s = spec("0 * * * *"); // every hour
        let next = s.next_after(now).unwrap();
        t.upsert(
            "a".into(),
            s,
            CatchupMode::Coalesce,
            chrono::Duration::zero(),
            None,
            Some(next),
        )
        .unwrap();
        let top = t.peek_next().unwrap();
        assert_eq!(top.id, "a");
        assert_eq!(top.fire_at, next);
    }

    #[test]
    fn upsert_with_earlier_time_reorders() {
        // Insert "later" first; upsert "earlier"; the earlier one must come
        // off the top. This is the architecture spec's "heap 重排在 upsert 后
        // 即时生效" requirement.
        let mut t = EntryTable::new();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let later = spec("0 12 * * *"); // noon
        let earlier = spec("0 1 * * *"); // 01:00
        let later_fire = later.next_after(now).unwrap();
        let earlier_fire = earlier.next_after(now).unwrap();
        t.upsert(
            "later".into(),
            later,
            CatchupMode::Coalesce,
            chrono::Duration::zero(),
            None,
            Some(later_fire),
        )
        .unwrap();
        t.upsert(
            "earlier".into(),
            earlier,
            CatchupMode::Coalesce,
            chrono::Duration::zero(),
            None,
            Some(earlier_fire),
        )
        .unwrap();
        let top = t.peek_next().unwrap();
        assert_eq!(top.id, "earlier");
    }

    #[test]
    fn rewriting_same_id_bumps_version_and_invalidates_old_heap_entry() {
        let mut t = EntryTable::new();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let original = spec("0 12 * * *");
        let original_next = original.next_after(now).unwrap();
        let v1 = t
            .upsert(
                "x".into(),
                original,
                CatchupMode::Coalesce,
                chrono::Duration::zero(),
                None,
                Some(original_next),
            )
            .unwrap();
        // Rewrite with a much earlier expression.
        let revised = spec("0 1 * * *");
        let revised_next = revised.next_after(now).unwrap();
        let v2 = t
            .upsert(
                "x".into(),
                revised,
                CatchupMode::Coalesce,
                chrono::Duration::zero(),
                None,
                Some(revised_next),
            )
            .unwrap();
        assert!(v2 > v1);
        // First pop must be the live one (revised_next), not the stale
        // original_next that's still physically in the heap.
        let top = t.pop_next().unwrap();
        assert_eq!(top.fire_at, revised_next);
        assert_eq!(top.version, v2);
        // Heap should now be empty after stale cleanup on the next peek.
        assert!(t.peek_next().is_none());
    }

    #[test]
    fn delete_makes_old_heap_entry_drop_on_pop() {
        let mut t = EntryTable::new();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let s = spec("0 12 * * *");
        let next = s.next_after(now).unwrap();
        t.upsert(
            "x".into(),
            s,
            CatchupMode::Coalesce,
            chrono::Duration::zero(),
            None,
            Some(next),
        )
        .unwrap();
        assert!(t.delete("x").is_some());
        assert!(t.peek_next().is_none());
        assert!(t.pop_next().is_none());
    }
}
