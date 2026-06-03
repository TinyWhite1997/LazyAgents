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
/// `failure_backoff` is intentionally excluded: it changes retry cadence after
/// failures, not the command, prompt, schedule, or budget of a successful run.
pub const SENSITIVE_CRON_FIELDS: &[CronSensitiveField] = &[
    CronSensitiveField::ProjectId,
    CronSensitiveField::BackendId,
    CronSensitiveField::SpawnArgs,
    CronSensitiveField::Prompt,
    CronSensitiveField::CronExpr,
    CronSensitiveField::Timezone,
    CronSensitiveField::MaxConcurrentRuns,
    CronSensitiveField::MaxRunsPerDay,
    CronSensitiveField::MaxRuntimeSeconds,
    CronSensitiveField::CostBudgetUsdPerDay,
    CronSensitiveField::PauseOnConsecutiveFailures,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CronSensitiveField {
    ProjectId,
    BackendId,
    SpawnArgs,
    Prompt,
    CronExpr,
    Timezone,
    MaxConcurrentRuns,
    MaxRunsPerDay,
    MaxRuntimeSeconds,
    CostBudgetUsdPerDay,
    PauseOnConsecutiveFailures,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CronSecuritySnapshot {
    pub project_id: String,
    pub backend_id: String,
    pub spawn_args: serde_json::Value,
    pub prompt: String,
    pub cron_expr: String,
    pub tz: String,
    pub max_concurrent_runs: i64,
    pub max_runs_per_day: i64,
    pub max_runtime_s: i64,
    pub cost_budget_usd_per_day: Option<f64>,
    pub pause_on_consecutive_failures: i64,
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
        if self.project_id != next.project_id {
            changed.push(CronSensitiveField::ProjectId);
        }
        if self.backend_id != next.backend_id {
            changed.push(CronSensitiveField::BackendId);
        }
        if self.spawn_args != next.spawn_args {
            changed.push(CronSensitiveField::SpawnArgs);
        }
        if self.prompt != next.prompt {
            changed.push(CronSensitiveField::Prompt);
        }
        if self.cron_expr != next.cron_expr {
            changed.push(CronSensitiveField::CronExpr);
        }
        if self.tz != next.tz {
            changed.push(CronSensitiveField::Timezone);
        }
        if self.max_concurrent_runs != next.max_concurrent_runs {
            changed.push(CronSensitiveField::MaxConcurrentRuns);
        }
        if self.max_runs_per_day != next.max_runs_per_day {
            changed.push(CronSensitiveField::MaxRunsPerDay);
        }
        if self.max_runtime_s != next.max_runtime_s {
            changed.push(CronSensitiveField::MaxRuntimeSeconds);
        }
        if self.cost_budget_usd_per_day != next.cost_budget_usd_per_day {
            changed.push(CronSensitiveField::CostBudgetUsdPerDay);
        }
        if self.pause_on_consecutive_failures != next.pause_on_consecutive_failures {
            changed.push(CronSensitiveField::PauseOnConsecutiveFailures);
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
        self.prune_expired(now);
        let Some(token) = token else {
            let issued = self.issue(cron_id, summary.clone(), now)?;
            return Ok(SetEnabledGate::RequiresConfirmation {
                token: issued,
                summary,
            });
        };

        let Some(pending) = self.pending.remove(token) else {
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
            project_id: "project".into(),
            backend_id: "claude".into(),
            spawn_args: serde_json::json!(["--model", "sonnet"]),
            prompt: prompt.into(),
            cron_expr: "17 3 * * *".into(),
            tz: "UTC".into(),
            max_concurrent_runs: 1,
            max_runs_per_day: 24,
            max_runtime_s: 1800,
            cost_budget_usd_per_day: Some(1.5),
            pause_on_consecutive_failures: 5,
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
    fn failure_backoff_is_not_a_sensitive_field() {
        assert!(!format!("{:?}", SENSITIVE_CRON_FIELDS).contains("FailureBackoff"));
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
    fn sensitive_upsert_invalidates_existing_tokens_for_that_cron() {
        let mut tokens = ConfirmationTokens::new();
        let now = Instant::now();
        let _ = tokens.issue("cron-a", summary(), now).unwrap();
        let _ = tokens.issue("cron-b", summary(), now).unwrap();

        assert_eq!(tokens.invalidate_cron("cron-a"), 1);
        assert_eq!(tokens.pending_len(), 1);
    }
}
