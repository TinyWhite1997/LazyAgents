//! `lad install` / `lad uninstall` orchestration.
//!
//! Single source of truth for verb ordering across the three back-ends:
//!
//! * `lad install --service <mode> [--enable] [--start]` runs
//!   `install` → optional `enable` → optional `start`. Each step is
//!   independent (A2): `--enable` does NOT imply `--start`, only
//!   "automatically follow up the install with an `enable` call".
//! * `lad uninstall --service <mode>` runs `disable` → `stop` →
//!   `uninstall`, all idempotent.
//!
//! Both functions return a slice of [`ActionOutcome`] so the CLI can
//! render every step. Errors short-circuit the sequence (we don't try
//! to `start` if `enable` blew up), but `Already` and `Skipped` are
//! both fine and continue.

use std::path::PathBuf;

use super::actions::ActionOutcome;
use super::launchd::LaunchdController;
use super::paths::{resolve_exec_path, resolve_home, resolve_user, InstallContext};
use super::systemd::SystemdController;
use super::windows_task::WindowsTaskController;
use super::{InstallError, InstallResult, ServiceController, ServiceMode};

/// Inputs for `lad install`. Constructed from the parsed CLI flags.
#[derive(Debug, Clone)]
pub struct InstallArgs {
    pub mode: ServiceMode,
    /// `--enable` — after install, mark the service enabled.
    pub enable: bool,
    /// `--start` — after install (and enable, if requested), start now.
    pub start: bool,
    /// Optional `--config` override; falls back to
    /// `la_config::resolve_config_path()` when None.
    pub config_path: Option<PathBuf>,
    /// `--dry-run` — log every action but don't write anything.
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct UninstallArgs {
    pub mode: ServiceMode,
    pub dry_run: bool,
}

/// Build an [`InstallContext`] from the active process env + the CLI
/// override (if any). Centralised so install + uninstall use identical
/// path resolution — otherwise the uninstall could try to remove a
/// file the installer never wrote.
pub fn build_context(
    mode: ServiceMode,
    config_override: Option<PathBuf>,
    dry_run: bool,
) -> InstallResult<InstallContext> {
    let exec_path = resolve_exec_path(if cfg!(windows) { "lad.exe" } else { "lad" })
        .map_err(|e| InstallError::Environment(format!("resolve exec path: {e}")))?;
    let home = resolve_home().map_err(|e| InstallError::Environment(e.to_string()))?;
    let user = resolve_user().map_err(|e| InstallError::Environment(e.to_string()))?;
    let config_path = match config_override {
        Some(p) => p,
        None => {
            let resolved = la_config::resolve_config_path();
            resolved
                .existing
                .clone()
                .unwrap_or_else(|| resolved.write_target.clone())
        }
    };
    // Hard guarantee for the service templates — the daemon process
    // must see an absolute path, otherwise launchd / systemd silently
    // fail to start it.
    if !exec_path.is_absolute() {
        return Err(InstallError::Environment(format!(
            "resolved exec path is not absolute: {}",
            exec_path.display()
        )));
    }
    if !config_path.is_absolute() {
        return Err(InstallError::Environment(format!(
            "resolved config path is not absolute: {}",
            config_path.display()
        )));
    }
    // Refuse `lad install --service launchd` on a host without
    // `$HOME/Library` — that catches a Linux runner that picked up
    // `--service launchd` by accident before we ever shell out to
    // `launchctl`.
    if matches!(mode, ServiceMode::Launchd) && !cfg!(target_os = "macos") {
        return Err(InstallError::Unsupported(
            "--service launchd is only valid on macOS hosts".to_string(),
        ));
    }
    if matches!(mode, ServiceMode::WindowsTask) && !cfg!(windows) {
        return Err(InstallError::Unsupported(
            "--service windows-task is only valid on Windows hosts".to_string(),
        ));
    }
    if matches!(mode, ServiceMode::Systemd) && !cfg!(target_os = "linux") {
        return Err(InstallError::Unsupported(
            "--service systemd is only valid on Linux hosts".to_string(),
        ));
    }
    Ok(InstallContext {
        exec_path,
        config_path,
        home,
        user,
        dry_run,
    })
}

fn controller_for(mode: ServiceMode, ctx: &InstallContext) -> Box<dyn ServiceController> {
    match mode {
        ServiceMode::Systemd => Box::new(SystemdController::from_ctx(ctx)),
        ServiceMode::Launchd => Box::new(LaunchdController::from_ctx(ctx)),
        ServiceMode::WindowsTask => Box::new(WindowsTaskController::from_ctx(ctx)),
    }
}

/// Run `lad install` end-to-end.
pub fn run_install(args: &InstallArgs) -> InstallResult<Vec<ActionOutcome>> {
    let ctx = build_context(args.mode, args.config_path.clone(), args.dry_run)?;
    let ctrl = controller_for(args.mode, &ctx);
    let mut log = Vec::with_capacity(3);
    log.push(ctrl.install(&ctx)?);
    if args.enable {
        log.push(ctrl.enable(&ctx)?);
    }
    if args.start {
        log.push(ctrl.start(&ctx)?);
    }
    Ok(log)
}

/// Run `lad uninstall` end-to-end. Order is `disable → stop →
/// uninstall` so we never try to delete a still-running task.
pub fn run_uninstall(args: &UninstallArgs) -> InstallResult<Vec<ActionOutcome>> {
    let ctx = build_context(args.mode, None, args.dry_run)?;
    let ctrl = controller_for(args.mode, &ctx);
    Ok(vec![
        ctrl.disable(&ctx)?,
        ctrl.stop(&ctx)?,
        ctrl.uninstall(&ctx)?,
    ])
}

/// Post-install message printed to stdout. Carries the R3 / A8
/// reminder for Windows tasks ("仅在你登录期间运行") plus the suggested
/// follow-up command if the user didn't pass `--enable` / `--start`.
pub fn render_post_install_hint(args: &InstallArgs) -> String {
    let mut lines = Vec::new();
    match args.mode {
        ServiceMode::Systemd => {
            lines.push("daemon installed as systemd user unit (lad.service).".to_string());
            if !args.enable {
                lines.push(
                    "hint: run `lad install --service systemd --enable` to start at login."
                        .to_string(),
                );
            }
            if !args.start {
                lines.push(
                    "hint: run `systemctl --user start lad.service` (or `lad install \
                     --service systemd --start`) to start the daemon now."
                        .to_string(),
                );
            }
        }
        ServiceMode::Launchd => {
            lines.push("daemon installed as launchd user agent (dev.lazyagents.lad).".to_string());
            if !args.enable {
                lines.push(
                    "hint: re-run `lad install --service launchd --enable` to bootstrap \
                     the agent into your login session."
                        .to_string(),
                );
            }
            if !args.start {
                lines.push(
                    "hint: run `launchctl kickstart -k gui/$UID/dev.lazyagents.lad` \
                     (or `lad install --service launchd --start`) to start now."
                        .to_string(),
                );
            }
        }
        ServiceMode::WindowsTask => {
            lines.push(
                "daemon installed as Windows Scheduled Task (\\LazyAgents\\lad).".to_string(),
            );
            // R3 / A8 contract — the user MUST know this.
            lines.push(
                "note: the task is registered at user logon only. The daemon \
                 will run only while you are logged in to this Windows account."
                    .to_string(),
            );
            lines.push("note: 任务在你登录期间运行；登出后 daemon 也会停止。".to_string());
            if !args.enable {
                lines.push(
                    "hint: re-run `lad install --service windows-task --enable` to \
                     enable start-at-logon."
                        .to_string(),
                );
            }
        }
    }
    lines.join("\n")
}
