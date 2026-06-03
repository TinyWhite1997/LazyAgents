//! Backend-only security state for cron enablement.
//!
//! UI confirmation is advisory. The daemon owns the token lifecycle and the
//! decision to enable, disable, or require reconfirmation.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

pub const CONFIRMATION_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Fields that change what unattended cron execution will do or spend.
///
/// Pinned by Rev2 §B4: exactly these eight fields gate the confirmation token
/// flow. Anything else (project_id, max_concurrent_runs, max_runtime_s,
/// pause_on_consecutive_failures, failure_backoff, …) is a non-sensitive edit
/// and must NOT require reconfirmation. The list is asserted against the brief
/// by `sensitive_fields_match_brief_rev2_b4` — adding or dropping a variant
/// here without updating that test (and the brief) breaks the build on
/// purpose.
pub const SENSITIVE_CRON_FIELDS: &[CronSensitiveField] = &[
    CronSensitiveField::Prompt,
    CronSensitiveField::BackendId,
    CronSensitiveField::SpawnArgs,
    CronSensitiveField::CronExpr,
    CronSensitiveField::Timezone,
    CronSensitiveField::CatchupMode,
    CronSensitiveField::MaxRunsPerDay,
    CronSensitiveField::CostBudgetUsdPerDay,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CronSensitiveField {
    Prompt,
    BackendId,
    SpawnArgs,
    CronExpr,
    Timezone,
    CatchupMode,
    MaxRunsPerDay,
    CostBudgetUsdPerDay,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CronSecuritySnapshot {
    pub backend_id: String,
    pub spawn_args: serde_json::Value,
    pub prompt: String,
    pub cron_expr: String,
    pub tz: String,
    pub catchup_mode: String,
    pub max_runs_per_day: i64,
    pub cost_budget_usd_per_day: Option<f64>,
}

impl CronSecuritySnapshot {
    pub fn validate(&self) -> Result<(), CronSecurityError> {
        if self.prompt.len() > MAX_PROMPT_BYTES {
            return Err(CronSecurityError::PromptTooLarge {
                actual: self.prompt.len(),
                limit: MAX_PROMPT_BYTES,
            });
        }
        Ok(())
    }

    pub fn changed_sensitive_fields(&self, next: &Self) -> Vec<CronSensitiveField> {
        let mut changed = Vec::new();
        if self.prompt != next.prompt {
            changed.push(CronSensitiveField::Prompt);
        }
        if self.backend_id != next.backend_id {
            changed.push(CronSensitiveField::BackendId);
        }
        if self.spawn_args != next.spawn_args {
            changed.push(CronSensitiveField::SpawnArgs);
        }
        if self.cron_expr != next.cron_expr {
            changed.push(CronSensitiveField::CronExpr);
        }
        if self.tz != next.tz {
            changed.push(CronSensitiveField::Timezone);
        }
        if self.catchup_mode != next.catchup_mode {
            changed.push(CronSensitiveField::CatchupMode);
        }
        if self.max_runs_per_day != next.max_runs_per_day {
            changed.push(CronSensitiveField::MaxRunsPerDay);
        }
        if self.cost_budget_usd_per_day != next.cost_budget_usd_per_day {
            changed.push(CronSensitiveField::CostBudgetUsdPerDay);
        }
        changed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertSecurityDecision {
    pub enabled_after_upsert: bool,
    pub requires_reconfirmation: bool,
    pub changed_fields: Vec<CronSensitiveField>,
}

pub fn decide_upsert_security(
    existing_enabled: bool,
    existing: Option<&CronSecuritySnapshot>,
    next: &CronSecuritySnapshot,
) -> Result<UpsertSecurityDecision, CronSecurityError> {
    next.validate()?;
    let existing_enabled = existing.is_some() && existing_enabled;
    let changed_fields = existing
        .map(|prev| prev.changed_sensitive_fields(next))
        .unwrap_or_default();
    let requires_reconfirmation = existing_enabled && !changed_fields.is_empty();
    Ok(UpsertSecurityDecision {
        enabled_after_upsert: existing_enabled && !requires_reconfirmation,
        requires_reconfirmation,
        changed_fields,
    })
}

#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationSummary {
    pub next_fire_at: Option<String>,
    pub budget: Option<String>,
    pub prompt_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationToken {
    token: SecretString,
}

impl ConfirmationToken {
    pub fn expose_secret(&self) -> &str {
        self.token.expose_secret()
    }
}

#[derive(Debug)]
struct PendingConfirmation {
    cron_id: String,
    expires_at: Instant,
    summary: ConfirmationSummary,
}

#[derive(Debug, Default)]
pub struct ConfirmationTokens {
    pending: HashMap<String, PendingConfirmation>,
}

impl ConfirmationTokens {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(
        &mut self,
        cron_id: impl Into<String>,
        summary: ConfirmationSummary,
        now: Instant,
    ) -> Result<ConfirmationToken, CronSecurityError> {
        self.prune_expired(now);
        let token = random_token()?;
        self.pending.insert(
            token.clone(),
            PendingConfirmation {
                cron_id: cron_id.into(),
                expires_at: now + CONFIRMATION_TOKEN_TTL,
                summary,
            },
        );
        Ok(ConfirmationToken {
            token: SecretString::new(token),
        })
    }

    pub fn require_or_confirm(
        &mut self,
        cron_id: &str,
        token: Option<&str>,
        summary: ConfirmationSummary,
        now: Instant,
    ) -> Result<SetEnabledGate, CronSecurityError> {
        let Some(token) = token else {
            self.prune_expired(now);
            let issued = self.issue(cron_id, summary.clone(), now)?;
            return Ok(SetEnabledGate::RequiresConfirmation {
                token: issued,
                summary,
            });
        };

        let Some(pending) = self.pending.remove(token) else {
            self.prune_expired(now);
            return Err(CronSecurityError::InvalidConfirmationToken);
        };
        if pending.expires_at <= now {
            return Err(CronSecurityError::ExpiredConfirmationToken);
        }
        if pending.cron_id != cron_id {
            return Err(CronSecurityError::TokenCronMismatch);
        }
        Ok(SetEnabledGate::Confirmed {
            summary: pending.summary,
        })
    }

    pub fn invalidate_cron(&mut self, cron_id: &str) -> usize {
        let before = self.pending.len();
        self.pending.retain(|_, p| p.cron_id != cron_id);
        before - self.pending.len()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn prune_expired(&mut self, now: Instant) {
        self.pending.retain(|_, p| p.expires_at > now);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetEnabledGate {
    RequiresConfirmation {
        token: ConfirmationToken,
        summary: ConfirmationSummary,
    },
    Confirmed {
        summary: ConfirmationSummary,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CronSecurityError {
    #[error("prompt is {actual} bytes; max is {limit}")]
    PromptTooLarge { actual: usize, limit: usize },
    #[error("invalid confirmation token")]
    InvalidConfirmationToken,
    #[error("confirmation token expired")]
    ExpiredConfirmationToken,
    #[error("confirmation token belongs to a different cron")]
    TokenCronMismatch,
    #[error("random token generation failed")]
    RandomToken,
}

fn random_token() -> Result<String, CronSecurityError> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| CronSecurityError::RandomToken)?;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> ConfirmationSummary {
        ConfirmationSummary {
            next_fire_at: Some("2026-06-03T03:17:00Z".into()),
            budget: Some("$1.50/day".into()),
            prompt_preview: "summarize".into(),
        }
    }

    fn snapshot(prompt: &str) -> CronSecuritySnapshot {
        CronSecuritySnapshot {
            backend_id: "claude".into(),
            spawn_args: serde_json::json!(["--model", "sonnet"]),
            prompt: prompt.into(),
            cron_expr: "17 3 * * *".into(),
            tz: "UTC".into(),
            catchup_mode: "coalesce".into(),
            max_runs_per_day: 24,
            cost_budget_usd_per_day: Some(1.5),
        }
    }

    #[test]
    fn secret_string_redacts_debug_and_display() {
        let s = SecretString::new("token-value");
        assert_eq!(format!("{s:?}"), "***");
        assert_eq!(s.to_string(), "***");
        assert_eq!(s.expose_secret(), "token-value");
    }

    #[test]
    fn enabled_cron_sensitive_change_auto_disables_and_requires_confirmation() {
        let prev = snapshot("old");
        let next = snapshot("new");
        let decision = decide_upsert_security(true, Some(&prev), &next).unwrap();

        assert!(!decision.enabled_after_upsert);
        assert!(decision.requires_reconfirmation);
        assert_eq!(decision.changed_fields, vec![CronSensitiveField::Prompt]);
    }

    #[test]
    fn new_cron_upsert_never_enables_in_one_step_even_if_caller_passes_enabled() {
        let decision = decide_upsert_security(true, None, &snapshot("new")).unwrap();

        assert!(!decision.enabled_after_upsert);
        assert!(!decision.requires_reconfirmation);
        assert!(decision.changed_fields.is_empty());
    }

    #[test]
    fn failure_backoff_is_not_a_sensitive_field() {
        assert!(!format!("{:?}", SENSITIVE_CRON_FIELDS).contains("FailureBackoff"));
    }

    #[test]
    fn sensitive_fields_match_brief_rev2_b4() {
        use std::collections::HashSet;

        // Rev2 §B4 enumerates exactly these eight sensitive fields. The set is
        // pinned here so any future addition or removal in
        // `SENSITIVE_CRON_FIELDS` must be made deliberately, alongside the
        // brief itself. Order is intentionally not asserted.
        let expected: HashSet<CronSensitiveField> = [
            CronSensitiveField::Prompt,
            CronSensitiveField::BackendId,
            CronSensitiveField::SpawnArgs,
            CronSensitiveField::CronExpr,
            CronSensitiveField::Timezone,
            CronSensitiveField::CatchupMode,
            CronSensitiveField::MaxRunsPerDay,
            CronSensitiveField::CostBudgetUsdPerDay,
        ]
        .into_iter()
        .collect();
        let actual: HashSet<CronSensitiveField> =
            SENSITIVE_CRON_FIELDS.iter().copied().collect();

        assert_eq!(
            actual, expected,
            "SENSITIVE_CRON_FIELDS must equal Rev2 §B4 exactly; \
             update both the brief and this test when changing the set"
        );
        assert_eq!(
            SENSITIVE_CRON_FIELDS.len(),
            8,
            "Rev2 §B4 pins exactly eight sensitive fields"
        );
    }

    #[test]
    fn oversized_prompt_is_rejected_at_backend_boundary() {
        let next = snapshot(&"x".repeat(MAX_PROMPT_BYTES + 1));
        let err = decide_upsert_security(false, None, &next).unwrap_err();
        assert_eq!(
            err,
            CronSecurityError::PromptTooLarge {
                actual: MAX_PROMPT_BYTES + 1,
                limit: MAX_PROMPT_BYTES
            }
        );
    }

    #[test]
    fn confirmation_token_is_single_use() {
        let mut tokens = ConfirmationTokens::new();
        let now = Instant::now();
        let first = tokens
            .require_or_confirm("cron-a", None, summary(), now)
            .unwrap();
        let token = match first {
            SetEnabledGate::RequiresConfirmation { token, .. } => token,
            SetEnabledGate::Confirmed { .. } => panic!("expected token"),
        };

        assert!(matches!(
            tokens
                .require_or_confirm("cron-a", Some(token.expose_secret()), summary(), now)
                .unwrap(),
            SetEnabledGate::Confirmed { .. }
        ));
        assert_eq!(tokens.pending_len(), 0);
        assert_eq!(
            tokens
                .require_or_confirm("cron-a", Some(token.expose_secret()), summary(), now)
                .unwrap_err(),
            CronSecurityError::InvalidConfirmationToken
        );
    }

    #[test]
    fn confirmation_token_cannot_enable_a_different_cron() {
        let mut tokens = ConfirmationTokens::new();
        let now = Instant::now();
        let token = match tokens
            .require_or_confirm("cron-a", None, summary(), now)
            .unwrap()
        {
            SetEnabledGate::RequiresConfirmation { token, .. } => token,
            SetEnabledGate::Confirmed { .. } => panic!("expected token"),
        };

        assert_eq!(
            tokens
                .require_or_confirm("cron-b", Some(token.expose_secret()), summary(), now)
                .unwrap_err(),
            CronSecurityError::TokenCronMismatch
        );
        assert_eq!(tokens.pending_len(), 0);
    }

    #[test]
    fn expired_confirmation_token_reports_expired() {
        let mut tokens = ConfirmationTokens::new();
        let now = Instant::now();
        let token = match tokens
            .require_or_confirm("cron-a", None, summary(), now)
            .unwrap()
        {
            SetEnabledGate::RequiresConfirmation { token, .. } => token,
            SetEnabledGate::Confirmed { .. } => panic!("expected token"),
        };

        assert_eq!(
            tokens
                .require_or_confirm(
                    "cron-a",
                    Some(token.expose_secret()),
                    summary(),
                    now + CONFIRMATION_TOKEN_TTL + Duration::from_secs(1),
                )
                .unwrap_err(),
            CronSecurityError::ExpiredConfirmationToken
        );
        assert_eq!(tokens.pending_len(), 0);
    }

    #[test]
    fn sensitive_upsert_invalidates_existing_tokens_for_that_cron() {
        let mut tokens = ConfirmationTokens::new();
        let now = Instant::now();
        let _ = tokens.issue("cron-a", summary(), now).unwrap();
        let _ = tokens.issue("cron-b", summary(), now).unwrap();

        assert_eq!(tokens.invalidate_cron("cron-a"), 1);
        assert_eq!(tokens.pending_len(), 1);
    }
}
