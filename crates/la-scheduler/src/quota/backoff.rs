//! `failure_backoff` DSL: parser + delay computation for per-cron retry
//! pacing (architecture §5.4).
//!
//! The DDL default literal is `'expo(1m,2,1h)'` (storage migration 0004),
//! meaning: first delay = 1m, multiply by 2 each consecutive failure,
//! capped at 1h. We accept the canonical `expo(BASE,FACTOR,CAP)` form;
//! anything else is a parse error (callers should fall back to the
//! DDL default rather than crash). v1 deliberately ships only `expo(...)`
//! — linear / fixed forms are out of scope.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Parsed shape of the `failure_backoff` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureBackoff {
    /// Exponential: `base * factor^(n-1)`, clamped to `cap`, where `n` is
    /// the consecutive-failure count after the failing run.
    Expo {
        base: Duration,
        factor: u32,
        cap: Duration,
    },
}

impl FailureBackoff {
    /// Delay before the next allowed fire after `consecutive_failures`
    /// terminal failures. `consecutive_failures = 0` returns `Duration::ZERO`
    /// — the scheduler isn't in a backoff window yet.
    pub fn delay_for(self, consecutive_failures: u32) -> Duration {
        match self {
            FailureBackoff::Expo { base, factor, cap } => {
                if consecutive_failures == 0 {
                    return Duration::ZERO;
                }
                // base * factor^(n-1), saturating at cap and at u64::MAX
                // micros. We compute in micros to keep header-room without
                // pulling in num-bigint.
                let exp = consecutive_failures.saturating_sub(1);
                let mut mul: u128 = 1;
                for _ in 0..exp {
                    mul = mul.saturating_mul(u128::from(factor.max(1)));
                    // If we've already passed the cap as a u128, stop;
                    // further iterations cannot bring us below it.
                    if mul > u128::from(u64::MAX) {
                        return cap;
                    }
                }
                let base_micros = base.as_micros();
                let scaled = base_micros.saturating_mul(mul);
                let cap_micros = cap.as_micros();
                let bounded = scaled.min(cap_micros);
                Duration::from_micros(u64::try_from(bounded).unwrap_or(u64::MAX))
            }
        }
    }
}

impl Default for FailureBackoff {
    fn default() -> Self {
        FailureBackoff::Expo {
            base: Duration::from_secs(60),
            factor: 2,
            cap: Duration::from_secs(3600),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BackoffParseError {
    #[error("unknown failure_backoff form (expected expo(BASE,FACTOR,CAP)): {0}")]
    UnknownForm(String),
    #[error("expected 3 arguments inside expo(...): {0}")]
    WrongArity(String),
    #[error("invalid factor (must be a positive integer): {0}")]
    BadFactor(String),
    #[error("invalid duration literal (expected '<int>(s|m|h)'): {0}")]
    BadDuration(String),
}

/// Parse a `failure_backoff` cell. Accepts whitespace inside the parens
/// and around commas because the DDL prose in §5.4 writes the literal as
/// `expo(1m, 2x, cap=1h)` but the DDL storage form is `expo(1m,2,1h)`
/// — we keep the strict storage form for the integer second arg, but
/// tolerate `2x` if the user typed the prose form by accident.
pub fn parse(raw: &str) -> Result<FailureBackoff, BackoffParseError> {
    let trimmed = raw.trim();
    let inside = trimmed
        .strip_prefix("expo(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| BackoffParseError::UnknownForm(raw.to_string()))?;
    let parts: Vec<&str> = inside.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        return Err(BackoffParseError::WrongArity(raw.to_string()));
    }
    let base = parse_duration(parts[0])?;
    let factor = parse_factor(parts[1])?;
    // Accept either `1h` or `cap=1h`.
    let cap_arg = parts[2].trim_start_matches("cap=").trim();
    let cap = parse_duration(cap_arg)?;
    Ok(FailureBackoff::Expo { base, factor, cap })
}

fn parse_factor(s: &str) -> Result<u32, BackoffParseError> {
    let stripped = s.trim().trim_end_matches('x');
    stripped
        .parse::<u32>()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| BackoffParseError::BadFactor(s.to_string()))
}

fn parse_duration(s: &str) -> Result<Duration, BackoffParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(BackoffParseError::BadDuration(s.to_string()));
    }
    let (num_part, unit) = s.split_at(s.len() - 1);
    let multiplier_secs: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(BackoffParseError::BadDuration(s.to_string())),
    };
    let value: u64 = num_part
        .parse()
        .map_err(|_| BackoffParseError::BadDuration(s.to_string()))?;
    Ok(Duration::from_secs(value.saturating_mul(multiplier_secs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ddl_default() {
        assert_eq!(
            parse("expo(1m,2,1h)").unwrap(),
            FailureBackoff::Expo {
                base: Duration::from_secs(60),
                factor: 2,
                cap: Duration::from_secs(3600),
            }
        );
    }

    #[test]
    fn parses_arch_prose_form_with_x_and_cap_prefix() {
        // §5.4 wrote `expo(1m, 2x, cap=1h)` in prose; we tolerate it so a
        // user editing config by hand doesn't get bitten.
        assert_eq!(
            parse("expo(1m, 2x, cap=1h)").unwrap(),
            FailureBackoff::Expo {
                base: Duration::from_secs(60),
                factor: 2,
                cap: Duration::from_secs(3600),
            }
        );
    }

    #[test]
    fn rejects_unknown_form() {
        assert!(matches!(
            parse("linear(1m,1m,1h)"),
            Err(BackoffParseError::UnknownForm(_))
        ));
        assert!(matches!(
            parse("expo 1m,2,1h"),
            Err(BackoffParseError::UnknownForm(_))
        ));
    }

    #[test]
    fn rejects_wrong_arity() {
        assert!(matches!(
            parse("expo(1m,2)"),
            Err(BackoffParseError::WrongArity(_))
        ));
        assert!(matches!(
            parse("expo(1m,2,1h,oops)"),
            Err(BackoffParseError::WrongArity(_))
        ));
    }

    #[test]
    fn rejects_bad_unit() {
        assert!(matches!(
            parse("expo(1d,2,1h)"),
            Err(BackoffParseError::BadDuration(_))
        ));
    }

    #[test]
    fn rejects_zero_factor() {
        assert!(matches!(
            parse("expo(1m,0,1h)"),
            Err(BackoffParseError::BadFactor(_))
        ));
    }

    #[test]
    fn delay_zero_for_no_failures() {
        let b = FailureBackoff::default();
        assert_eq!(b.delay_for(0), Duration::ZERO);
    }

    #[test]
    fn delay_grows_exponentially_then_caps() {
        let b = FailureBackoff::Expo {
            base: Duration::from_secs(60),
            factor: 2,
            cap: Duration::from_secs(3600),
        };
        // failure 1 → 60s, failure 2 → 120s, failure 3 → 240s, … failure 7 → 3840s capped to 3600
        assert_eq!(b.delay_for(1), Duration::from_secs(60));
        assert_eq!(b.delay_for(2), Duration::from_secs(120));
        assert_eq!(b.delay_for(3), Duration::from_secs(240));
        assert_eq!(b.delay_for(6), Duration::from_secs(1920));
        assert_eq!(b.delay_for(7), Duration::from_secs(3600));
        assert_eq!(b.delay_for(100), Duration::from_secs(3600));
    }

    #[test]
    fn very_large_consecutive_failures_does_not_overflow() {
        let b = FailureBackoff::Expo {
            base: Duration::from_secs(60),
            factor: 2,
            cap: Duration::from_secs(3600),
        };
        // Saturating math — should clamp to cap, not panic / wrap.
        let d = b.delay_for(u32::MAX);
        assert_eq!(d, Duration::from_secs(3600));
    }
}
