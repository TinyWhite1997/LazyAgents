//! Cron expression parsing with IANA timezone.
//!
//! Wraps the `cron` crate to give us:
//! - 5-field (`m h dom mon dow`) input is auto-rewritten to the 6-field form
//!   the underlying crate expects, by prefixing `"0 "` (fire at second 0).
//! - 6-field (`s m h dom mon dow`) input is passed through untouched.
//! - 7-field (`s m h dom mon dow year`) input is rejected — the architecture
//!   doc only specs 5 and 6, and accepting more silently invites surprises.
//! - Timezone resolution against `chrono-tz`, so DST transitions follow IANA
//!   rules rather than fixed offsets (§5.1).

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use std::str::FromStr;

use crate::error::Error;

/// A parsed cron expression bound to a specific IANA timezone.
#[derive(Debug, Clone)]
pub struct CronSpec {
    schedule: Schedule,
    tz: Tz,
    /// Original user input, retained for diagnostics.
    raw: String,
}

impl CronSpec {
    /// Parse `expr` (5 or 6 fields) and resolve `tz` against IANA. Both errors
    /// map onto the `CRON_INVALID_*` IPC codes defined in la-proto.
    pub fn parse(expr: &str, tz: &str) -> Result<Self, Error> {
        let normalised = normalise_expr(expr)?;
        let schedule = Schedule::from_str(&normalised).map_err(|e| Error::InvalidExpr {
            raw: expr.to_string(),
            reason: e.to_string(),
        })?;
        let tz: Tz = tz
            .parse()
            .map_err(|_| Error::InvalidTimezone(tz.to_string()))?;
        Ok(Self {
            schedule,
            tz,
            raw: expr.to_string(),
        })
    }

    /// User-visible representation (original 5- or 6-field string).
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Resolved IANA timezone.
    pub fn tz(&self) -> Tz {
        self.tz
    }

    /// First fire time strictly after `after` (UTC).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // Convert the cutoff into the spec's IANA tz so DST is honoured, then
        // ask cron for the next fire and convert back to UTC for storage /
        // heap ordering.
        let after_tz = after.with_timezone(&self.tz);
        self.schedule
            .after(&after_tz)
            .next()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// All fire times in the half-open interval `(start, end]`, in chronological
    /// order. Used by the catch-up path to enumerate missed fires after a
    /// daemon restart or clock jump.
    ///
    /// Stops collecting after `limit` entries — callers downstream still need
    /// to apply [`crate::catchup::MAX_CATCHUP`], but a per-iterator cap keeps
    /// pathological expressions ("every second since 1970") from hanging the
    /// thread.
    pub fn fires_between(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Vec<DateTime<Utc>> {
        let start_tz = start.with_timezone(&self.tz);
        let end_tz = end.with_timezone(&self.tz);
        let mut out = Vec::new();
        for fire in self.schedule.after(&start_tz) {
            if fire > end_tz {
                break;
            }
            out.push(fire.with_timezone(&Utc));
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Preview the next `n` fire times after `after`. Powers `crons.dry_run`
    /// in the IPC surface (§5.6).
    pub fn preview(&self, after: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
        let after_tz = after.with_timezone(&self.tz);
        self.schedule
            .after(&after_tz)
            .take(n)
            .map(|dt| dt.with_timezone(&Utc))
            .collect()
    }
}

/// Normalise user-typed `expr` into the 6-field form the `cron` crate accepts.
///
/// We split on ASCII whitespace, count fields, and either pass through (6) or
/// prepend `"0"` for the seconds slot (5). Anything else is a hard error so a
/// typo like a missing space doesn't get silently coerced.
fn normalise_expr(expr: &str) -> Result<String, Error> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidExpr {
            raw: expr.to_string(),
            reason: "expression is empty".to_string(),
        });
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    match fields.len() {
        5 => Ok(format!("0 {}", fields.join(" "))),
        6 => Ok(fields.join(" ")),
        n => Err(Error::InvalidExpr {
            raw: expr.to_string(),
            reason: format!("expected 5 or 6 fields, got {n}"),
        }),
    }
}

/// Helper: parse a wall-time literal in `tz` and convert to UTC. Exposed for
/// IPC callers that need to interpret strings like `"2026-03-08 02:30:00"`
/// as "in this timezone".
///
/// Returns `Err(Error::InvalidExpr)` for non-existent local times (the
/// spring-forward gap, e.g. `2026-03-08 02:30 America/Los_Angeles`) and for
/// the second occurrence of an ambiguous fall-back hour — the first
/// occurrence is returned via `earliest()` to match cron crate behaviour.
pub fn wall_time_in_tz(
    tz: Tz,
    y: i32,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
) -> Result<DateTime<Utc>, Error> {
    let mapped = tz.with_ymd_and_hms(y, mo, d, h, mi, s);
    let resolved = mapped.single().or_else(|| mapped.earliest());
    match resolved {
        Some(dt) => Ok(dt.with_timezone(&Utc)),
        None => Err(Error::InvalidExpr {
            raw: format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
            reason: format!("wall time does not exist in timezone {tz:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_field_is_promoted_to_six() {
        // every minute, UTC
        let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 30).unwrap();
        let next = spec.next_after(t0).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 12, 1, 0).unwrap());
    }

    #[test]
    fn six_field_passes_through() {
        // 30 seconds into every minute
        let spec = CronSpec::parse("30 * * * * *", "UTC").unwrap();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let next = spec.next_after(t0).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 30).unwrap());
    }

    #[test]
    fn seven_fields_is_rejected() {
        // The cron crate accepts a year field; we deliberately do not.
        let err = CronSpec::parse("0 0 12 * * * 2026", "UTC").unwrap_err();
        assert!(matches!(err, Error::InvalidExpr { .. }));
    }

    #[test]
    fn bad_tz_maps_to_invalid_tz() {
        let err = CronSpec::parse("* * * * *", "Mars/Olympus_Mons").unwrap_err();
        assert!(matches!(err, Error::InvalidTimezone(_)));
    }

    #[test]
    fn between_returns_missed_fires() {
        let spec = CronSpec::parse("0 * * * *", "UTC").unwrap(); // top of every hour
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 1, 1, 13, 30, 0).unwrap();
        let fires = spec.fires_between(start, end, 16);
        assert_eq!(
            fires,
            vec![
                Utc.with_ymd_and_hms(2026, 1, 1, 11, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 1, 1, 13, 0, 0).unwrap(),
            ]
        );
    }

    #[test]
    fn between_respects_limit() {
        let spec = CronSpec::parse("* * * * *", "UTC").unwrap();
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let end = start + chrono::Duration::days(7);
        let fires = spec.fires_between(start, end, 5);
        assert_eq!(fires.len(), 5);
    }
}
