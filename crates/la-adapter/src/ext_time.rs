//! RFC3339 timestamp helpers used by adapter `discover()` paths to
//! report a session's wall-clock creation time without pulling in
//! `chrono`. Mirrors the algorithm used by `la-daemon::health` —
//! advisory only, leap seconds ignored.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Format `t` as `YYYY-MM-DDTHH:MM:SSZ`. Returns `None` if `t` predates
/// the Unix epoch (clock skew).
pub fn system_time_to_rfc3339(t: SystemTime) -> Option<String> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let (year, month, day, hour, minute, second) = unix_to_ymdhms(secs);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Read `path`'s mtime and format as RFC3339. Returns `None` on any
/// I/O failure (missing file, EACCES, …) — caller treats it as a
/// best-effort hint.
pub fn file_mtime_rfc3339(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mt = meta.modified().ok()?;
    system_time_to_rfc3339(mt)
}

fn unix_to_ymdhms(unix: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (unix / 86_400) as i64;
    let secs_today = (unix % 86_400) as u32;
    let hour = secs_today / 3600;
    let minute = (secs_today % 3600) / 60;
    let second = secs_today % 60;

    // Days from 1970-01-01 — Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = (y + i64::from(m <= 2)) as i32;
    (y, m as u32, d as u32, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(
            system_time_to_rfc3339(UNIX_EPOCH).unwrap(),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn known_timestamp_round_trips() {
        // 2026-06-03T00:00:00Z = 1_780_444_800 (verified via `date -u -d`).
        let t = UNIX_EPOCH + Duration::from_secs(1_780_444_800);
        assert_eq!(system_time_to_rfc3339(t).unwrap(), "2026-06-03T00:00:00Z");
    }

    #[test]
    fn before_epoch_returns_none() {
        // Construct a SystemTime that's earlier than UNIX_EPOCH so the
        // duration_since(UNIX_EPOCH) call inside is Err.
        let t = UNIX_EPOCH.checked_sub(Duration::from_secs(1)).unwrap();
        assert!(system_time_to_rfc3339(t).is_none());
    }

    #[test]
    fn missing_path_returns_none() {
        assert!(file_mtime_rfc3339(Path::new("/nonexistent/lazyagents-test-path")).is_none());
    }
}
