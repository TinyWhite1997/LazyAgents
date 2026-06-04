//! `lad install` / `lad uninstall` — service manager wiring for
//! systemd, launchd, and Windows Scheduled Task (WEK-73 / M4.1).
//!
//! Hard rules from the M4 brief v1.1 addendum:
//!
//! * **A2** — three-OS verb table (install / enable / start / stop /
//!   disable / uninstall) is **orthogonal**. `--enable` only marks the
//!   service enabled; it never starts. `--start` does start; never enables.
//! * **A3** — launchd plist: absolute exec path (no `~`, no `$HOME`),
//!   `RunAtLoad=false`, `KeepAlive=true`, explicit `EnvironmentVariables`
//!   including `LAZYAGENTS_MANAGED_BY=launchd`.
//! * **A8** — Windows Scheduled Task, never SCM service. `LogonTrigger`
//!   only, `Principal` = installing user (never SYSTEM).
//! * **S1** — every service unit exports `LAZYAGENTS_MANAGED_BY` so the
//!   `la` bootstrap sees the daemon is managed and skips auto-daemonize.
//!
//! All three back-ends share one orchestrator: [`run_install`] /
//! [`run_uninstall`] in [`crate::install::cli`] do the verb sequencing
//! and idempotency reporting; each [`ServiceController`] only knows the
//! one-shot primitive verbs for its platform.

pub mod actions;
pub mod cli;
pub mod launchd;
pub mod paths;
pub mod systemd;
pub mod template;
pub mod windows_task;

pub use actions::{ActionOutcome, ServiceVerb};
pub use cli::{run_install, run_uninstall, InstallArgs, UninstallArgs};
pub use paths::{
    detect_running_service, resolve_exec_path, resolve_home, resolve_user, InstallContext,
    LaunchdPaths, SystemdPaths, WindowsTaskPaths,
};
pub use template::{render_template, TemplateError};

/// One-stop error type for the service install layer. We use a
/// hand-rolled enum instead of pulling in `anyhow`: the install path
/// has a small fixed set of failure modes and surfacing them as typed
/// variants makes the CLI tests easier to write.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("io error at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("template error: {0}")]
    Template(#[from] TemplateError),
    #[error("service manager command `{command}` failed: {detail}")]
    Command { command: String, detail: String },
    #[error("service manager command `{command}` not found on PATH")]
    CommandMissing { command: String },
    #[error("environment lookup failed: {0}")]
    Environment(String),
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
}

impl InstallError {
    pub fn io(path: impl Into<std::path::PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
    pub fn command(command: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Command {
            command: command.into(),
            detail: detail.into(),
        }
    }
}

pub type InstallResult<T> = std::result::Result<T, InstallError>;

/// Which service manager the user asked us to wire into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceMode {
    /// systemd user unit (`~/.config/systemd/user/lad.service`).
    Systemd,
    /// launchd user agent (`~/Library/LaunchAgents/dev.lazyagents.lad.plist`).
    Launchd,
    /// Windows Scheduled Task at user logon (`\LazyAgents\lad`).
    WindowsTask,
}

impl ServiceMode {
    /// Parse the `--service <mode>` CLI value. Accepts `systemd`,
    /// `launchd`, `windows-task`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "systemd" => Ok(Self::Systemd),
            "launchd" => Ok(Self::Launchd),
            "windows-task" | "windows" => Ok(Self::WindowsTask),
            other => Err(format!(
                "unknown --service value: {other} (accepted: systemd | launchd | windows-task)"
            )),
        }
    }

    /// Wire-format string for `LAZYAGENTS_MANAGED_BY`.
    pub fn managed_by_tag(&self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
            Self::WindowsTask => "windows-task",
        }
    }
}

/// Trait implemented by each per-OS controller. Every verb is required;
/// "not applicable" paths return [`ActionOutcome::Skipped`] rather than
/// erroring so the orchestrator can keep marching through `install +
/// enable + start` without a special case.
pub trait ServiceController {
    /// Materialise the unit file / plist / task XML on disk. The
    /// `existed` field of the outcome lets the orchestrator print
    /// "WARN: already installed" instead of "INSTALLED" on a re-run.
    fn install(&self, ctx: &paths::InstallContext) -> InstallResult<ActionOutcome>;

    /// Mark the service enabled (start-at-login). Idempotent.
    fn enable(&self, ctx: &paths::InstallContext) -> InstallResult<ActionOutcome>;

    /// Start the service now. Idempotent.
    fn start(&self, ctx: &paths::InstallContext) -> InstallResult<ActionOutcome>;

    /// Stop the service now. Idempotent (no-op if not running).
    fn stop(&self, ctx: &paths::InstallContext) -> InstallResult<ActionOutcome>;

    /// Mark the service disabled (do not start at login). Idempotent.
    fn disable(&self, ctx: &paths::InstallContext) -> InstallResult<ActionOutcome>;

    /// Remove unit file + tell the service manager. Idempotent.
    fn uninstall(&self, ctx: &paths::InstallContext) -> InstallResult<ActionOutcome>;

    /// Human-readable mode label, used in CLI output.
    fn mode(&self) -> ServiceMode;
}
