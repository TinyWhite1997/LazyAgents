//! Windows Scheduled Task controller (`\LazyAgents\lad`).
//!
//! M4 brief v1.1 addendum A8 / R3 hard rules:
//!
//! * Always a per-user Scheduled Task with `LogonTrigger` — never a
//!   Windows SCM service. This avoids the SYSTEM-level surface area an
//!   SCM service would expose and matches "run while the user is
//!   logged in" semantics that the rest of the daemon assumes.
//! * `Principal` = current user, `RunLevel=LeastPrivilege`. We
//!   **refuse** to install if the resolved user is `SYSTEM` (some CI
//!   runners drop us under the system account by accident).
//! * `Enabled=false` on first install; flipped on by the `enable` verb
//!   via `schtasks /Change /TN ... /ENABLE`.
//! * Sane `RestartOnFailure` (3 retries, 1 minute apart) without
//!   `AllowHardTerminate` so a stop never SIGKILLs an in-flight run.
//!
//! Verbs all go through `schtasks.exe`; we do not link the COM Task
//! Scheduler interface so a cross-compile from Linux still produces a
//! working binary.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use super::actions::{ActionOutcome, ServiceVerb};
use super::paths::{InstallContext, WindowsTaskPaths};
use super::template::render_template;
use super::{InstallError, InstallResult, ServiceController, ServiceMode};

pub const TEMPLATE: &str = include_str!("../../templates/lad-task.xml");

#[derive(Debug, Clone)]
pub struct WindowsTaskController {
    pub paths: WindowsTaskPaths,
}

impl WindowsTaskController {
    pub fn from_ctx(ctx: &InstallContext) -> Self {
        let appdata_buf = std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from);
        let appdata_ref = appdata_buf.as_deref();
        Self {
            paths: WindowsTaskPaths::from_appdata_or_home(appdata_ref, &ctx.home),
        }
    }

    pub fn rendered_task_xml(&self, ctx: &InstallContext) -> InstallResult<String> {
        // Refuse SYSTEM accounts up-front — A8 forbids them.
        let user_lower = ctx.user.to_ascii_lowercase();
        if user_lower == "system" || user_lower.ends_with("\\system") {
            return Err(InstallError::Environment(format!(
                "refusing to install Scheduled Task as `{}` — A8 requires a \
                 non-SYSTEM, non-admin user principal",
                ctx.user
            )));
        }

        let exec = ctx.exec_path.to_string_lossy().to_string();
        let config = ctx.config_path.to_string_lossy().to_string();
        let working = self.paths.working_dir.to_string_lossy().to_string();

        let mut vars = BTreeMap::new();
        vars.insert("user_id", ctx.user.as_str());
        vars.insert("exec_path", exec.as_str());
        vars.insert("config_path", config.as_str());
        vars.insert("working_dir", working.as_str());
        let rendered = render_template(TEMPLATE, &vars)?;
        // Belt-and-braces: the template must keep `<Enabled>false</Enabled>`
        // on install (A8). If a future edit drops that flag, fail loudly.
        if !rendered.contains("<Enabled>false</Enabled>") {
            return Err(InstallError::Environment(
                "rendered Windows task XML must include <Enabled>false</Enabled> \
                 on install — A8 requires explicit enable via --enable / schtasks /ENABLE"
                    .to_string(),
            ));
        }
        Ok(rendered)
    }

    fn task_exists(&self) -> bool {
        require_schtasks(&["/Query", "/TN", &self.paths.task_name])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl ServiceController for WindowsTaskController {
    fn mode(&self) -> ServiceMode {
        ServiceMode::WindowsTask
    }

    fn install(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        let rendered = self.rendered_task_xml(ctx)?;
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Install,
                format!(
                    "would write {} and import via schtasks /Create",
                    self.paths.xml_path.display()
                ),
            ));
        }
        std::fs::create_dir_all(&self.paths.xml_dir)
            .map_err(|e| InstallError::io(&self.paths.xml_dir, e))?;
        write_atomically(&self.paths.xml_path, rendered.as_bytes())?;
        // `schtasks /Create /XML <file> /TN <name> /F` overwrites an
        // existing task, which is the idempotent install we want.
        let out = require_schtasks(&[
            "/Create",
            "/XML",
            &self.paths.xml_path.to_string_lossy(),
            "/TN",
            &self.paths.task_name,
            "/F",
        ])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Install,
                format!(
                    "schtasks /Create /XML {} /TN {} /F",
                    self.paths.xml_path.display(),
                    self.paths.task_name
                ),
            ))
        } else {
            Err(InstallError::command(
                "schtasks /Create",
                out.stderr_string(),
            ))
        }
    }

    fn enable(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Enable,
                format!(
                    "would run schtasks /Change /TN {} /ENABLE",
                    self.paths.task_name
                ),
            ));
        }
        let out = require_schtasks(&["/Change", "/TN", &self.paths.task_name, "/ENABLE"])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Enable,
                format!("schtasks /Change /TN {} /ENABLE", self.paths.task_name),
            ))
        } else {
            Err(InstallError::command(
                "schtasks /Change /ENABLE",
                out.stderr_string(),
            ))
        }
    }

    fn start(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Start,
                format!("would run schtasks /Run /TN {}", self.paths.task_name),
            ));
        }
        if !self.task_exists() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Start,
                format!("{} not installed", self.paths.task_name),
            ));
        }
        let out = require_schtasks(&["/Run", "/TN", &self.paths.task_name])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Start,
                format!("schtasks /Run /TN {}", self.paths.task_name),
            ))
        } else {
            let stderr = out.stderr_string();
            // schtasks reports "is currently running" as success on
            // most Windows builds, but at least one (Win Server 2019)
            // returns 1; treat that case as idempotent.
            if stderr.contains("already running") || stderr.contains("currently running") {
                Ok(ActionOutcome::already(
                    ServiceVerb::Start,
                    format!("{} already running", self.paths.task_name),
                ))
            } else {
                Err(InstallError::command("schtasks /Run", stderr))
            }
        }
    }

    fn stop(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Stop,
                format!("would run schtasks /End /TN {}", self.paths.task_name),
            ));
        }
        if !self.task_exists() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Stop,
                format!("{} not installed", self.paths.task_name),
            ));
        }
        let out = require_schtasks(&["/End", "/TN", &self.paths.task_name])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Stop,
                format!("schtasks /End /TN {}", self.paths.task_name),
            ))
        } else {
            let stderr = out.stderr_string();
            if stderr.contains("not running") || stderr.contains("no instance") {
                Ok(ActionOutcome::already(
                    ServiceVerb::Stop,
                    format!("{} not running", self.paths.task_name),
                ))
            } else {
                Err(InstallError::command("schtasks /End", stderr))
            }
        }
    }

    fn disable(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Disable,
                format!(
                    "would run schtasks /Change /TN {} /DISABLE",
                    self.paths.task_name
                ),
            ));
        }
        if !self.task_exists() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Disable,
                format!("{} not installed", self.paths.task_name),
            ));
        }
        let out = require_schtasks(&["/Change", "/TN", &self.paths.task_name, "/DISABLE"])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Disable,
                format!("schtasks /Change /TN {} /DISABLE", self.paths.task_name),
            ))
        } else {
            Err(InstallError::command(
                "schtasks /Change /DISABLE",
                out.stderr_string(),
            ))
        }
    }

    fn uninstall(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Uninstall,
                format!(
                    "would run schtasks /Delete /TN {} /F and rm {}",
                    self.paths.task_name,
                    self.paths.xml_path.display()
                ),
            ));
        }
        let mut steps = Vec::new();
        if self.task_exists() {
            let out = require_schtasks(&["/Delete", "/TN", &self.paths.task_name, "/F"])?;
            if !out.status.success() {
                return Err(InstallError::command(
                    "schtasks /Delete",
                    out.stderr_string(),
                ));
            }
            steps.push(format!("schtasks /Delete /TN {} /F", self.paths.task_name));
        }
        if self.paths.xml_path.exists() {
            std::fs::remove_file(&self.paths.xml_path)
                .map_err(|e| InstallError::io(&self.paths.xml_path, e))?;
            steps.push(format!("removed {}", self.paths.xml_path.display()));
        }
        if steps.is_empty() {
            Ok(ActionOutcome::already(
                ServiceVerb::Uninstall,
                format!("{} not installed", self.paths.task_name),
            ))
        } else {
            Ok(ActionOutcome::done(
                ServiceVerb::Uninstall,
                steps.join("; "),
            ))
        }
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> InstallResult<()> {
    use std::io::Write as _;
    let parent = path.parent().ok_or_else(|| {
        InstallError::io(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"),
        )
    })?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("lad-task")
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    write_result.map_err(|e| InstallError::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| InstallError::io(path, e))
}

#[derive(Debug)]
struct SchtasksOutput {
    status: std::process::ExitStatus,
    #[allow(dead_code)]
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl SchtasksOutput {
    fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

fn require_schtasks(args: &[&str]) -> InstallResult<SchtasksOutput> {
    let mut cmd = Command::new("schtasks");
    for a in args {
        cmd.arg(a);
    }
    match cmd.output() {
        Ok(out) => Ok(SchtasksOutput {
            status: out.status,
            stdout: out.stdout,
            stderr: out.stderr,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(InstallError::CommandMissing {
            command: "schtasks".to_string(),
        }),
        Err(e) => Err(InstallError::command("schtasks", e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> InstallContext {
        InstallContext {
            exec_path: PathBuf::from(r"C:\Users\alice\.cargo\bin\lad.exe"),
            config_path: PathBuf::from(r"C:\Users\alice\AppData\Roaming\lazyagents\config.toml"),
            home: PathBuf::from(r"C:\Users\alice"),
            user: "WORKGROUP\\alice".to_string(),
            dry_run: true,
        }
    }

    #[test]
    fn task_xml_has_enabled_false_and_logon_trigger() {
        let c = ctx();
        let ctrl = WindowsTaskController::from_ctx(&c);
        let rendered = ctrl.rendered_task_xml(&c).unwrap();
        // A8 hard rule: Enabled=false on install.
        assert!(rendered.contains("<Enabled>false</Enabled>"));
        // LogonTrigger — never a SCM service trigger.
        assert!(rendered.contains("<LogonTrigger>"));
        // Principal: the installing user, LeastPrivilege.
        assert!(rendered.contains("<UserId>WORKGROUP\\alice</UserId>"));
        assert!(rendered.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        // RestartOnFailure at least one retry.
        assert!(rendered.contains("<RestartOnFailure>"));
        // AllowHardTerminate=false.
        assert!(rendered.contains("<AllowHardTerminate>false</AllowHardTerminate>"));
    }

    #[test]
    fn refuses_system_principal() {
        let mut c = ctx();
        c.user = "SYSTEM".to_string();
        let ctrl = WindowsTaskController::from_ctx(&c);
        let err = ctrl.rendered_task_xml(&c).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("SYSTEM"), "{msg}");
    }

    #[test]
    fn rendered_xml_declaration_matches_file_encoding() {
        // The file is written as raw UTF-8 bytes (`rendered.as_bytes()`),
        // so the XML declaration MUST advertise UTF-8 — otherwise
        // `schtasks /Create /XML` refuses the file on real Windows.
        let c = ctx();
        let ctrl = WindowsTaskController::from_ctx(&c);
        let rendered = ctrl.rendered_task_xml(&c).unwrap();
        let first_line = rendered.lines().next().unwrap();
        assert!(
            first_line.contains("encoding=\"UTF-8\""),
            "XML declaration must say UTF-8 (matches as_bytes() write): {first_line:?}"
        );
        // And the body must be valid UTF-8 (sanity, since as_bytes() can't fail).
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
    }

    #[test]
    fn clean_host_uninstall_round_trip_is_idempotent() {
        // On a clean host (no `schtasks.exe`, or task never installed),
        // `disable` / `stop` / `uninstall` must all short-circuit to
        // `Already`. We can't shell out to a real schtasks from this
        // unit test, but we can exercise the early-return branch by
        // pointing `task_exists()` at a name that doesn't resolve and
        // observing that no command is dispatched.
        //
        // `task_exists()` already returns `false` when `schtasks` is
        // missing or the query fails — that's enough to prove the
        // idempotent path; integration coverage on real Windows lives
        // in WEK-72's matrix CI.
        let c = ctx();
        let mut ctrl = WindowsTaskController::from_ctx(&c);
        ctrl.paths.task_name =
            "\\LazyAgents\\definitely-does-not-exist-wek73-idempotency".to_string();

        // task_exists() must report false (no such task / schtasks missing).
        assert!(
            !ctrl.task_exists(),
            "phantom task must not exist for this test to be meaningful"
        );

        // Build a non-dry-run ctx so we go through the real branches.
        let mut live = ctx();
        live.dry_run = false;
        for outcome in [
            ctrl.disable(&live)
                .expect("disable must not error on clean host"),
            ctrl.stop(&live).expect("stop must not error on clean host"),
            ctrl.uninstall(&live)
                .expect("uninstall must not error on clean host"),
        ] {
            assert!(
                matches!(outcome, ActionOutcome::Already { .. }),
                "expected Already on clean host, got {outcome:?}"
            );
        }
    }
}
