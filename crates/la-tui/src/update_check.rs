//! `la --check-update` — pull the latest GitHub Release manifest and
//! compare against the running binary's compile-time version. Default
//! posture: print and exit; **never auto-download or auto-install**.
//!
//! The WEK-41 acceptance line "自动升级开关默认 off（避免 cron 中切版本）"
//! is the load-bearing constraint here: a daemon-spawning client must
//! not silently swap binaries underneath a running `lad`, because a
//! mid-cron version flip means in-flight sessions speak a different
//! wire protocol than the daemon that owns them. So this module is
//! deliberately a *check*, not an *updater*.
//!
//! Output is plain text on stdout (single line for the "up to date"
//! case, three lines for the "newer release available" case) plus a
//! short hint about how to actually install if the user wants to.
//!
//! Network failures are *non-fatal*: we print a short note to stderr
//! and exit 0, so a `--check-update` baked into a wrapper script does
//! not start failing the moment the user is offline.

use std::io::{self, Write};
use std::time::Duration;

use serde::Deserialize;

/// Compile-time crate version. `clap` would normally surface this, but
/// we hand-roll the flag (see `bin/la.rs`) to keep the dep tree small.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default manifest endpoint. Overridable via env so dev / integration
/// can point at a fixture; the production code path never hits a host
/// other than github.com.
const DEFAULT_MANIFEST_URL: &str =
    "https://api.github.com/repos/TinyWhite1997/LazyAgents/releases/latest";
const MANIFEST_URL_ENV: &str = "LAZYAGENTS_UPDATE_MANIFEST_URL";

/// User-Agent: GitHub's API rejects requests without one. Include the
/// running version so server-side analytics can see adoption velocity
/// of `--check-update` itself (handy for the M4 GA exit metric review).
fn user_agent() -> String {
    format!(
        "la/{} (+https://github.com/TinyWhite1997/LazyAgents)",
        CURRENT_VERSION
    )
}

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    /// GitHub's "release name" — typically empty for tag-only releases,
    /// so we always fall back to `tag_name`.
    #[serde(default)]
    name: String,
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    prerelease: bool,
}

impl ReleaseManifest {
    /// Strip a leading `v` so `v0.2.0` and `0.2.0` compare equal.
    fn normalized_version(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }

    fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.tag_name
        } else {
            &self.name
        }
    }
}

/// Outcome surfaced to the caller. The binary translates these into
/// exit codes; the human-readable text is rendered by [`render`].
#[derive(Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate {
        current: String,
        latest: String,
    },
    UpdateAvailable {
        current: String,
        latest: String,
        url: String,
    },
    /// Network / parse failure. The string is the short user-facing
    /// reason; we do not exit non-zero on this — see module docs.
    Unavailable(String),
}

/// Perform the HTTP fetch + comparison. Bounded to ~5s end-to-end so a
/// flaky network never wedges the binary at startup.
pub fn check_for_update() -> CheckOutcome {
    check_with_url(&manifest_url())
}

fn manifest_url() -> String {
    std::env::var(MANIFEST_URL_ENV).unwrap_or_else(|_| DEFAULT_MANIFEST_URL.to_string())
}

fn check_with_url(url: &str) -> CheckOutcome {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .user_agent(&user_agent())
        .build();

    let response = match agent
        .get(url)
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => r,
        Err(e) => return CheckOutcome::Unavailable(format!("network: {e}")),
    };

    let manifest: ReleaseManifest = match response.into_json() {
        Ok(m) => m,
        Err(e) => return CheckOutcome::Unavailable(format!("parse manifest: {e}")),
    };

    if manifest.prerelease {
        // We never advertise a prerelease as "the" upgrade target — the
        // user can opt into prereleases manually, but `--check-update`
        // is the "stable channel" command. Reporting "up to date" here
        // keeps cron-style wrappers quiet on prerelease churn.
        return CheckOutcome::UpToDate {
            current: CURRENT_VERSION.to_string(),
            latest: CURRENT_VERSION.to_string(),
        };
    }

    let latest = manifest.normalized_version().to_string();
    if is_newer(&latest, CURRENT_VERSION) {
        CheckOutcome::UpdateAvailable {
            current: CURRENT_VERSION.to_string(),
            latest,
            url: if manifest.html_url.is_empty() {
                format!(
                    "https://github.com/TinyWhite1997/LazyAgents/releases/tag/{}",
                    manifest.display_name()
                )
            } else {
                manifest.html_url
            },
        }
    } else {
        CheckOutcome::UpToDate {
            current: CURRENT_VERSION.to_string(),
            latest,
        }
    }
}

/// SemVer-aware comparison without pulling in the `semver` crate. We
/// split on `.` / `-` and compare numeric segments numerically; any
/// non-numeric tail (`-rc.1`, `-pre`) is treated as *older* than the
/// same major.minor.patch with no suffix — matching how GitHub orders
/// SemVer tags. This is enough for the check-update use case; the
/// installer side of cargo-dist owns the strict parsing.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        // If parsing fails on either side, fall back to a string compare;
        // worst case is we annoy the user with one stale "available" hit.
        _ => candidate != current && candidate > current,
    }
}

/// Returns `(major, minor, patch, has_no_prerelease)`. The fourth tuple
/// element flips a release ahead of any `-rc` / `-pre` at the same
/// numeric version.
fn parse_version(v: &str) -> Option<(u64, u64, u64, bool)> {
    let (numeric, suffix) = match v.split_once('-') {
        Some((n, s)) => (n, Some(s)),
        None => (v, None),
    };
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch, suffix.is_none()))
}

/// Render to any writer — the binary passes stdout; tests pass a Vec.
pub fn render(outcome: &CheckOutcome, w: &mut impl Write) -> io::Result<()> {
    match outcome {
        CheckOutcome::UpToDate { current, latest } => {
            writeln!(w, "la {current} is up to date (latest release: {latest}).")?;
        }
        CheckOutcome::UpdateAvailable {
            current,
            latest,
            url,
        } => {
            writeln!(w, "la {current} → {latest} available.")?;
            writeln!(w, "Release notes: {url}")?;
            writeln!(
                w,
                "Install: re-run the install.sh from the release, or `brew upgrade lazyagents` / `scoop update la`."
            )?;
        }
        CheckOutcome::Unavailable(reason) => {
            // stderr — see module docs: this is non-fatal.
            writeln!(w, "la --check-update: could not reach GitHub ({reason}).")?;
        }
    }
    Ok(())
}

/// Process exit code for `--check-update`:
///   0 — up to date OR unreachable (non-fatal, see module docs)
///   1 — would only ever be returned on a logic bug; reserved
///   2 — newer release available (so wrapper scripts can detect it)
pub fn exit_code(outcome: &CheckOutcome) -> u8 {
    match outcome {
        CheckOutcome::UpToDate { .. } | CheckOutcome::Unavailable(_) => 0,
        CheckOutcome::UpdateAvailable { .. } => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_pure_semver() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn release_beats_same_version_prerelease() {
        // 0.2.0 final ranks above 0.2.0-rc.1, per WEK-41 acceptance
        // ("默认不自动升级"): the user must never be pushed onto a -rc
        // unless they opted in.
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
    }

    #[test]
    fn render_up_to_date() {
        let mut buf = Vec::new();
        render(
            &CheckOutcome::UpToDate {
                current: "0.1.0".into(),
                latest: "0.1.0".into(),
            },
            &mut buf,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("0.1.0"));
        assert!(s.contains("up to date"));
    }

    #[test]
    fn render_update_available() {
        let mut buf = Vec::new();
        render(
            &CheckOutcome::UpdateAvailable {
                current: "0.1.0".into(),
                latest: "0.2.0".into(),
                url: "https://example.test/r/v0.2.0".into(),
            },
            &mut buf,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("0.1.0 → 0.2.0"));
        assert!(s.contains("Release notes"));
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            exit_code(&CheckOutcome::UpToDate {
                current: "0.1.0".into(),
                latest: "0.1.0".into()
            }),
            0
        );
        assert_eq!(
            exit_code(&CheckOutcome::UpdateAvailable {
                current: "0.1.0".into(),
                latest: "0.2.0".into(),
                url: "x".into(),
            }),
            2
        );
        assert_eq!(exit_code(&CheckOutcome::Unavailable("offline".into())), 0);
    }
}
