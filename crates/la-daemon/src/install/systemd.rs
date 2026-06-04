//! systemd user unit controller (`lad.service`).
//!
//! All verbs go through `systemctl --user`. We avoid running anything
//! as root: the M4 brief is explicit that we want a per-user supervised
//! daemon. If the host has no systemd at all (Linux container with only
//! `/sbin/init`, WSL1, …) the controller still writes the unit file —
//! the install is a no-op in practice but the file is there for when
//! the user moves to a systemd-managed host.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use super::actions::{ActionOutcome, ServiceVerb};
use super::paths::{InstallContext, SystemdPaths};
use super::template::render_template;
use super::{InstallError, InstallResult, ServiceController, ServiceMode};

/// Bundled at compile time — the canonical template ships with the
/// daemon binary so an offline install still works.
pub const TEMPLATE: &str = include_str!("../../templates/lad.service");

#[derive(Debug, Clone)]
pub struct SystemdController {
    pub paths: SystemdPaths,
}

impl SystemdController {
    pub fn from_ctx(ctx: &InstallContext) -> Self {
        Self {
            paths: SystemdPaths::from_home(&ctx.home),
        }
    }

    pub fn rendered_unit(&self, ctx: &InstallContext) -> InstallResult<String> {
        let exec = ctx.exec_path.to_string_lossy().to_string();
        let config = ctx.config_path.to_string_lossy().to_string();
        let mut vars = BTreeMap::new();
        vars.insert("exec_path", exec.as_str());
        vars.insert("config_path", config.as_str());
        Ok(render_template(TEMPLATE, &vars)?)
    }

    fn is_enabled(&self) -> bool {
        run_systemctl(&["is-enabled", SystemdPaths::UNIT_NAME])
            .map(|out| out.status.success() && out.stdout_trim_eq("enabled"))
            .unwrap_or(false)
    }

    fn is_active(&self) -> bool {
        run_systemctl(&["is-active", SystemdPaths::UNIT_NAME])
            .map(|out| out.status.success() && out.stdout_trim_eq("active"))
            .unwrap_or(false)
    }
}

impl ServiceController for SystemdController {
    fn mode(&self) -> ServiceMode {
        ServiceMode::Systemd
    }

    fn install(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        let rendered = self.rendered_unit(ctx)?;
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Install,
                format!("would write {}", self.paths.unit_path.display()),
            ));
        }
        std::fs::create_dir_all(&self.paths.unit_dir)
            .map_err(|e| InstallError::io(&self.paths.unit_dir, e))?;
        let prior = if self.paths.unit_path.exists() {
            std::fs::read_to_string(&self.paths.unit_path).ok()
        } else {
            None
        };
        if prior.as_deref() == Some(rendered.as_str()) {
            return Ok(ActionOutcome::already(
                ServiceVerb::Install,
                format!("unchanged at {}", self.paths.unit_path.display()),
            ));
        }
        write_atomically(&self.paths.unit_path, rendered.as_bytes())?;
        // systemd needs to re-scan the user unit directory before it
        // sees a new file. `daemon-reload` returns non-zero on hosts
        // without a user systemd instance; downgrade those to a debug
        // outcome so the install verb still succeeds.
        let _ = run_systemctl(&["daemon-reload"]);
        Ok(ActionOutcome::done(
            ServiceVerb::Install,
            format!("wrote {}", self.paths.unit_path.display()),
        ))
    }

    fn enable(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Enable,
                "would run systemctl --user enable lad.service".to_string(),
            ));
        }
        if self.is_enabled() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Enable,
                "lad.service already enabled".to_string(),
            ));
        }
        let out = require_systemctl(&["enable", SystemdPaths::UNIT_NAME])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Enable,
                "systemctl --user enable lad.service".to_string(),
            ))
        } else {
            Err(InstallError::command(
                "systemctl --user enable lad.service",
                out.stderr_string(),
            ))
        }
    }

    fn start(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Start,
                "would run systemctl --user start lad.service".to_string(),
            ));
        }
        if self.is_active() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Start,
                "lad.service already active".to_string(),
            ));
        }
        let out = require_systemctl(&["start", SystemdPaths::UNIT_NAME])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Start,
                "systemctl --user start lad.service".to_string(),
            ))
        } else {
            Err(InstallError::command(
                "systemctl --user start lad.service",
                out.stderr_string(),
            ))
        }
    }

    fn stop(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Stop,
                "would run systemctl --user stop lad.service".to_string(),
            ));
        }
        if !self.is_active() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Stop,
                "lad.service not active".to_string(),
            ));
        }
        let out = require_systemctl(&["stop", SystemdPaths::UNIT_NAME])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Stop,
                "systemctl --user stop lad.service".to_string(),
            ))
        } else {
            Err(InstallError::command(
                "systemctl --user stop lad.service",
                out.stderr_string(),
            ))
        }
    }

    fn disable(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Disable,
                "would run systemctl --user disable lad.service".to_string(),
            ));
        }
        if !self.is_enabled() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Disable,
                "lad.service already disabled".to_string(),
            ));
        }
        let out = require_systemctl(&["disable", SystemdPaths::UNIT_NAME])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Disable,
                "systemctl --user disable lad.service".to_string(),
            ))
        } else {
            Err(InstallError::command(
                "systemctl --user disable lad.service",
                out.stderr_string(),
            ))
        }
    }

    fn uninstall(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Uninstall,
                format!("would rm {}", self.paths.unit_path.display()),
            ));
        }
        if !self.paths.unit_path.exists() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Uninstall,
                format!(
                    "no unit file at {} (nothing to remove)",
                    self.paths.unit_path.display()
                ),
            ));
        }
        std::fs::remove_file(&self.paths.unit_path)
            .map_err(|e| InstallError::io(&self.paths.unit_path, e))?;
        let _ = run_systemctl(&["daemon-reload"]);
        Ok(ActionOutcome::done(
            ServiceVerb::Uninstall,
            format!("removed {}", self.paths.unit_path.display()),
        ))
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> InstallResult<()> {
    use std::io::Write as _;
    let parent = path.parent().ok_or_else(|| {
        InstallError::io(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("path {} has no parent", path.display()),
            ),
        )
    })?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("lad-service")
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    write_result.map_err(|e| InstallError::io(&tmp, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::metadata(&tmp).map_err(|e| InstallError::io(&tmp, e))?;
        let mut perms = meta.permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&tmp, perms).map_err(|e| InstallError::io(&tmp, e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| InstallError::io(path, e))
}

#[derive(Debug)]
struct SystemctlOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl SystemctlOutput {
    fn stdout_trim_eq(&self, want: &str) -> bool {
        let s = String::from_utf8_lossy(&self.stdout);
        s.trim() == want
    }
    fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

fn run_systemctl(args: &[&str]) -> std::io::Result<SystemctlOutput> {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user");
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output()?;
    Ok(SystemctlOutput {
        status: out.status,
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

fn require_systemctl(args: &[&str]) -> InstallResult<SystemctlOutput> {
    match run_systemctl(args) {
        Ok(out) => Ok(out),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(InstallError::CommandMissing {
            command: "systemctl".to_string(),
        }),
        Err(e) => Err(InstallError::command("systemctl", e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn template_renders_with_absolute_paths_and_managed_by_env() {
        let ctx = InstallContext {
            exec_path: PathBuf::from("/usr/local/bin/lad"),
            config_path: PathBuf::from("/home/u/.config/lazyagents/config.toml"),
            home: PathBuf::from("/home/u"),
            user: "u".to_string(),
            dry_run: true,
        };
        let ctrl = SystemdController::from_ctx(&ctx);
        let rendered = ctrl.rendered_unit(&ctx).unwrap();
        assert!(rendered.contains(
            "ExecStart=/usr/local/bin/lad start --config /home/u/.config/lazyagents/config.toml"
        ));
        assert!(rendered.contains("Environment=LAZYAGENTS_MANAGED_BY=systemd"));
        assert!(rendered
            .contains("Environment=LAZYAGENTS_CONFIG=/home/u/.config/lazyagents/config.toml"));
        // Sanity: no leftover placeholder tokens.
        assert!(!rendered.contains("{{"));
    }
}
