//! Common result types for service install/uninstall verbs.
//!
//! Every primitive verb (install / enable / start / stop / disable /
//! uninstall) returns an [`ActionOutcome`] that distinguishes "we did
//! the thing" from "already in the requested state" from "skipped
//! because the manager isn't applicable here". The CLI uses the
//! distinction to render `INSTALLED` vs `WARN: already installed` vs
//! the no-op debug line — and to keep its exit code at 0 in all three
//! cases (idempotency requirement, A2 verb table).

use std::fmt;

/// A single verb in the orthogonal A2 table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceVerb {
    Install,
    Enable,
    Start,
    Stop,
    Disable,
    Uninstall,
}

impl ServiceVerb {
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Enable => "enable",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Disable => "disable",
            Self::Uninstall => "uninstall",
        }
    }
}

impl fmt::Display for ServiceVerb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What happened when we ran a verb. Drives the CLI's stdout / exit
/// code — `Already` and `Skipped` are both successes (idempotency), the
/// difference is only in how loudly we tell the user.
#[derive(Debug, Clone)]
pub enum ActionOutcome {
    /// The verb performed its action. `detail` is a short human note
    /// (path written, command executed, ...).
    Done { verb: ServiceVerb, detail: String },
    /// The verb's effect was already in place — printed as `WARN: ...`
    /// so the user sees the no-op, but exit stays 0.
    Already { verb: ServiceVerb, detail: String },
    /// The verb does not apply on this platform / in this context
    /// (e.g. `disable` for a Windows task that was never enabled).
    /// Printed at debug level only.
    Skipped { verb: ServiceVerb, detail: String },
}

impl ActionOutcome {
    pub fn done(verb: ServiceVerb, detail: impl Into<String>) -> Self {
        Self::Done {
            verb,
            detail: detail.into(),
        }
    }

    pub fn already(verb: ServiceVerb, detail: impl Into<String>) -> Self {
        Self::Already {
            verb,
            detail: detail.into(),
        }
    }

    pub fn skipped(verb: ServiceVerb, detail: impl Into<String>) -> Self {
        Self::Skipped {
            verb,
            detail: detail.into(),
        }
    }

    pub fn verb(&self) -> ServiceVerb {
        match self {
            Self::Done { verb, .. } | Self::Already { verb, .. } | Self::Skipped { verb, .. } => {
                *verb
            }
        }
    }

    /// CLI label — `OK` / `WARN` / `SKIP`.
    pub fn cli_tag(&self) -> &'static str {
        match self {
            Self::Done { .. } => "OK",
            Self::Already { .. } => "WARN",
            Self::Skipped { .. } => "SKIP",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::Done { detail, .. }
            | Self::Already { detail, .. }
            | Self::Skipped { detail, .. } => detail,
        }
    }
}

impl fmt::Display for ActionOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.cli_tag(), self.verb(), self.detail())
    }
}
