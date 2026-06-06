//! WEK-53: the single backend serializer for every `crons.*` mutation.
//!
//! The dispatcher's `crons.upsert / crons.set_enabled / crons.delete`
//! handlers MUST route through [`CronControl`] rather than touching
//! `storage.crons()` directly. Two invariants hang off that:
//!
//! 1. **`cron_security` is non-bypassable.** Every mutation snapshots the
//!    existing row first, runs [`crate::cron_security::decide_upsert_security`]
//!    or [`ConfirmationTokens::require_or_confirm`], and only then commits
//!    to SQLite. The dispatcher can't accidentally skip the check because
//!    the only paths into the `crons` table from a handler context are the
//!    methods on this struct.
//! 2. **Storage and the scheduler heap can't diverge.** A handler that
//!    wrote the row first and the heap second (or vice versa) without
//!    serialization could let a second concurrent request observe one side
//!    of the pair before the other side caught up — the scheduler heap
//!    starts firing on stale spec, or the SQLite snapshot says enabled
//!    while the heap has no entry. [`CronControl`] holds a single
//!    `tokio::Mutex` across (security decision → storage write → heap
//!    upsert) for each cron id; concurrent mutations queue rather than
//!    interleave. The heap itself is updated via the
//!    [`SchedulerHandle`] mpsc — the "scheduler control channel" the
//!    WEK-53 brief names — so the heap mutation is also serialized on the
//!    scheduler side.
//!
//! ## What goes in the lock
//!
//! Only the (decide → write → heap) triple for one cron. The lock is
//! short-held: the storage write is a single SQLite UPSERT and the heap
//! call awaits a oneshot reply from the scheduler loop. We do not hold
//! the lock across `crons.run_now` (that's the executor's admission lock
//! and a different mutex) or across the fire executor (those happen
//! outside the IPC dispatcher entirely).
//!
//! ## Why a `Mutex<()>` instead of an mpsc
//!
//! The brief calls for a "control channel" — the existing
//! [`SchedulerHandle`] mpsc already serializes the heap side. What was
//! missing is serializing the *storage* side and the security state
//! machine. A mutex is the smallest piece that gives that without adding
//! a second background task to wait on shutdown. Future expansion (a
//! command enum behind an mpsc) can swap the mutex out without changing
//! the handler-facing API on this struct.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use la_scheduler::{catchup::CatchupMode, CronSpec};
use la_storage::{Cron, CronUpsert, Storage};
use tokio::sync::Mutex;

use crate::cron_security::{
    decide_upsert_security, ConfirmationSummary, ConfirmationToken, ConfirmationTokens,
    CronSecurityError, CronSecuritySnapshot, SetEnabledGate,
};
use crate::scheduler::{
    cron_to_security_snapshot, parse_cron_spec_public, parse_sqlite_lexical_utc_public,
    REPLAY_INTERVAL_FLOOR_PUBLIC,
};
use la_scheduler::SchedulerHandle;

/// Outcome of a `set_enabled` call that the dispatcher must translate to
/// the wire `CronsSetEnabledResult`.
#[derive(Debug)]
pub enum SetEnabledOutcome {
    /// First-step of the two-step enable: a fresh token was issued.
    /// `cron.enabled` is unchanged in storage and the heap.
    RequiresConfirmation {
        cron: Cron,
        token: ConfirmationToken,
        summary: ConfirmationSummary,
    },
    /// Either `enabled = false` (always allowed) or `enabled = true` with
    /// a valid token. Storage and heap are now consistent.
    Applied { cron: Cron },
}

/// Outcome of an `upsert` call.
#[derive(Debug)]
pub struct UpsertOutcome {
    pub cron: Cron,
    /// True when this upsert hit a sensitive field on an already-enabled
    /// cron, causing the daemon to force-disable it and invalidate any
    /// pending confirmation token. The dispatcher surfaces this on the
    /// wire so the UI can prompt the user to re-enable.
    pub requires_reconfirmation: bool,
}

/// Errors surfaced from a control-channel mutation.
#[derive(Debug, thiserror::Error)]
pub enum CronControlError {
    #[error("cron not found: {0}")]
    NotFound(String),
    #[error("invalid cron expression: {0}")]
    InvalidExpr(String),
    #[error("invalid timezone: {0}")]
    InvalidTz(String),
    #[error("storage: {0}")]
    Storage(#[from] la_storage::StorageError),
    #[error("security: {0}")]
    Security(#[from] CronSecurityError),
    #[error("{0}")]
    Other(String),
}

/// Single serialization point for every `crons.*` write. Cheap to clone:
/// all state lives behind `Arc`s.
#[derive(Clone)]
pub struct CronControl {
    inner: Arc<Inner>,
}

struct Inner {
    storage: Storage,
    handle: SchedulerHandle,
    /// Held across (security decision → storage write → heap upsert) so
    /// the three steps look atomic from the perspective of another
    /// concurrent handler. The unit-typed mutex is intentional: nothing
    /// inside the lock needs to be carried out of it; we just need a
    /// happens-before edge.
    lock: Mutex<()>,
    /// In-memory single-use confirmation tokens. Hashed by token string;
    /// see [`ConfirmationTokens`] for the lifecycle.
    tokens: Mutex<ConfirmationTokens>,
}

impl CronControl {
    pub fn new(storage: Storage, handle: SchedulerHandle) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage,
                handle,
                lock: Mutex::new(()),
                tokens: Mutex::new(ConfirmationTokens::new()),
            }),
        }
    }

    /// Snapshot of pending tokens (test hook). Production handlers do not
    /// inspect this — they go through [`Self::set_enabled`].
    pub async fn pending_token_count(&self) -> usize {
        self.inner.tokens.lock().await.pending_len()
    }

    /// Upsert a cron through the control channel.
    ///
    /// - **New cron** (no existing row): the row is written with
    ///   `enabled = false` regardless of what the caller asked for. Per
    ///   §22 of the security doc, no single call may both create and
    ///   enable a cron. The caller must follow up with `set_enabled`,
    ///   which will issue a confirmation token.
    /// - **Existing enabled cron, no sensitive field changed**: the row is
    ///   updated in place; `enabled` is preserved; the heap is re-upserted
    ///   so a `failure_backoff` / non-sensitive edit takes effect.
    /// - **Existing enabled cron, at least one sensitive field changed**:
    ///   the row is updated AND force-disabled; any pending confirmation
    ///   token for this cron is invalidated; the heap entry is removed so
    ///   the cron stops firing until the caller re-enables.
    /// - **Existing disabled cron**: write the row; the heap stays empty;
    ///   sensitive-field tracking is moot until enable is attempted.
    pub async fn upsert(&self, mut upsert: CronUpsert) -> Result<UpsertOutcome, CronControlError> {
        // Pre-parse the expression / tz before taking the lock. A bad
        // spec must never write a row; pre-parsing also lets us reuse
        // the parsed `spec`/`mode` inside the lock without a second
        // `CronSpec::parse` allocation.
        let spec = CronSpec::parse(&upsert.cron_expr, &upsert.tz).map_err(|err| match err {
            la_scheduler::Error::InvalidExpr { reason, .. } => {
                CronControlError::InvalidExpr(reason)
            }
            la_scheduler::Error::InvalidTimezone(tz) => CronControlError::InvalidTz(tz),
            other => CronControlError::Other(other.to_string()),
        })?;
        let mode = catchup_mode_from_str(&upsert.catchup_mode);

        let _guard = self.inner.lock.lock().await;

        // Snapshot existing row inside the lock so we can't race a
        // concurrent upsert of the same id.
        let existing = self.inner.storage.crons().get(&upsert.id).await?;
        let existing_enabled = existing.as_ref().map(|c| c.enabled != 0).unwrap_or(false);

        // Build the (existing, next) security snapshots and decide.
        let existing_snapshot = existing.as_ref().map(cron_to_security_snapshot);
        let next_snapshot = upsert_to_security_snapshot(&upsert);
        let decision =
            decide_upsert_security(existing_enabled, existing_snapshot.as_ref(), &next_snapshot)?;

        // Force the enabled flag according to the security decision. The
        // dispatcher's existing pattern of carrying the prior `enabled`
        // forward is encoded in the same place now.
        upsert.enabled = decision.enabled_after_upsert;

        // Storage write — single SQLite UPSERT inside the control lock.
        let cron = self.inner.storage.crons().upsert(upsert.clone()).await?;

        // Heap reconciliation: enabled rows must be installed; disabled
        // rows (including the freshly force-disabled case) must NOT be
        // in the heap.
        if cron.enabled != 0 {
            let last_fired = cron
                .last_fired_at
                .as_deref()
                .and_then(parse_sqlite_lexical_utc_public);
            self.inner
                .handle
                .upsert(
                    cron.id.clone(),
                    spec.clone(),
                    mode,
                    REPLAY_INTERVAL_FLOOR_PUBLIC,
                    last_fired,
                )
                .await
                .map_err(|e| CronControlError::Other(e.to_string()))?;
        } else {
            // delete is idempotent — fine if the entry was never in the heap.
            let _ = self.inner.handle.delete(cron.id.clone()).await;
        }

        // Sensitive-field edit on a previously-enabled cron invalidates
        // any pending token. Done AFTER the storage+heap pair so a
        // failure earlier doesn't drop tokens the caller might still
        // legitimately redeem against the unchanged row.
        let requires_reconfirmation = decision.requires_reconfirmation;
        if requires_reconfirmation {
            let _ = self.inner.tokens.lock().await.invalidate_cron(&cron.id);
        }

        Ok(UpsertOutcome {
            cron,
            requires_reconfirmation,
        })
    }

    /// Two-step enable. First call with `token = None` issues a fresh
    /// single-use confirmation token; second call with that token applies
    /// the enable. Disable (`enabled = false`) is always allowed in one
    /// step and bypasses the token machinery.
    pub async fn set_enabled(
        &self,
        cron_id: &str,
        enabled: bool,
        token: Option<&str>,
        now: Instant,
    ) -> Result<SetEnabledOutcome, CronControlError> {
        let _guard = self.inner.lock.lock().await;

        // Always load the existing row inside the lock so the security
        // summary (prompt preview / next fire / budget) reflects the
        // committed state, not a stale snapshot the dispatcher captured
        // before queueing up.
        let cron = self
            .inner
            .storage
            .crons()
            .get(cron_id)
            .await?
            .ok_or_else(|| CronControlError::NotFound(cron_id.to_string()))?;

        if !enabled {
            // Disable path: drop pending tokens, flip storage, remove
            // from heap. No token is consulted; a user (or an autopilot)
            // can always pause a runaway cron.
            let _ = self.inner.tokens.lock().await.invalidate_cron(cron_id);
            self.inner
                .storage
                .crons()
                .set_enabled(cron_id, false)
                .await?;
            let _ = self.inner.handle.delete(cron_id.to_string()).await;
            let updated = self
                .inner
                .storage
                .crons()
                .get(cron_id)
                .await?
                .ok_or_else(|| CronControlError::NotFound(cron_id.to_string()))?;
            return Ok(SetEnabledOutcome::Applied { cron: updated });
        }

        // Enable path: consult the token state machine.
        let summary = build_confirmation_summary(&cron);
        let gate = {
            let mut tokens = self.inner.tokens.lock().await;
            tokens.require_or_confirm(cron_id, token, summary.clone(), now)?
        };
        match gate {
            SetEnabledGate::RequiresConfirmation { token, summary } => {
                // No storage / heap mutation. The cron stays as-is until
                // the caller comes back with the token.
                Ok(SetEnabledOutcome::RequiresConfirmation {
                    cron,
                    token,
                    summary,
                })
            }
            SetEnabledGate::Confirmed { summary: _ } => {
                // Validate spec one more time before flipping; the cron
                // row may have a bad expr/tz that landed during an
                // earlier disabled upsert.
                let (spec, mode, throttle) =
                    parse_cron_spec_public(&cron).map_err(CronControlError::Other)?;
                self.inner
                    .storage
                    .crons()
                    .set_enabled(cron_id, true)
                    .await?;
                let last_fired = cron
                    .last_fired_at
                    .as_deref()
                    .and_then(parse_sqlite_lexical_utc_public);
                self.inner
                    .handle
                    .upsert(cron.id.clone(), spec, mode, throttle, last_fired)
                    .await
                    .map_err(|e| CronControlError::Other(e.to_string()))?;
                let updated = self
                    .inner
                    .storage
                    .crons()
                    .get(cron_id)
                    .await?
                    .ok_or_else(|| CronControlError::NotFound(cron_id.to_string()))?;
                Ok(SetEnabledOutcome::Applied { cron: updated })
            }
        }
    }

    /// Delete a cron through the control channel. Invalidates any pending
    /// confirmation token, removes the heap entry, and deletes the
    /// storage row. Returns whether a row was deleted.
    pub async fn delete(&self, cron_id: &str) -> Result<bool, CronControlError> {
        let _guard = self.inner.lock.lock().await;
        let _ = self.inner.tokens.lock().await.invalidate_cron(cron_id);
        let removed = self.inner.storage.crons().delete(cron_id).await?;
        let _ = self.inner.handle.delete(cron_id.to_string()).await;
        Ok(removed)
    }
}

fn catchup_mode_from_str(s: &str) -> CatchupMode {
    match s {
        "skip" => CatchupMode::Skip,
        "replay" => CatchupMode::Replay,
        _ => CatchupMode::Coalesce,
    }
}

fn upsert_to_security_snapshot(upsert: &CronUpsert) -> CronSecuritySnapshot {
    CronSecuritySnapshot {
        backend_id: upsert.backend_id.clone(),
        spawn_args: upsert.spawn_args.clone(),
        prompt: upsert.prompt.clone(),
        cron_expr: upsert.cron_expr.clone(),
        tz: upsert.tz.clone(),
        catchup_mode: upsert.catchup_mode.clone(),
        max_runs_per_day: upsert.max_runs_per_day,
        cost_budget_usd_per_day: upsert.cost_budget_usd_per_day,
    }
}

fn build_confirmation_summary(cron: &Cron) -> ConfirmationSummary {
    // Prompt preview: 64 chars is enough to spot-check intent ("summarize
    // logs" vs "delete the database") without leaking the full body.
    const PROMPT_PREVIEW_CHARS: usize = 64;
    let prompt_preview: String = cron.prompt.chars().take(PROMPT_PREVIEW_CHARS).collect();
    let next_fire_at = next_fire_at_for_summary(cron);
    let budget = cron
        .cost_budget_usd_per_day
        .map(|usd| format!("${usd:.2}/day"));
    ConfirmationSummary {
        next_fire_at,
        budget,
        prompt_preview,
    }
}

fn next_fire_at_for_summary(cron: &Cron) -> Option<String> {
    let spec = CronSpec::parse(&cron.cron_expr, &cron.tz).ok()?;
    let now: DateTime<Utc> = Utc::now();
    spec.next_after(now).map(|dt| dt.to_rfc3339())
}
