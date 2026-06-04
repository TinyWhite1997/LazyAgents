//! launchd user-agent controller (`dev.lazyagents.lad.plist`).
//!
//! M4 brief v1.1 addendum A3 hard rules:
//!
//! * Absolute exec path baked into `ProgramArguments[0]` — no `~`,
//!   `$HOME`, `$XDG_*`, no shell variables (launchd never expands them).
//! * `--config` argument is an absolute config path resolved by
//!   `la-config::resolve_config_path()`, also baked in.
//! * `EnvironmentVariables` enumerates `LAZYAGENTS_MANAGED_BY=launchd`,
//!   `LAZYAGENTS_CONFIG`, `HOME`, `USER`, and a minimal `PATH`.
//! * `RunAtLoad=false` + `KeepAlive=true` so launchd does not bring the
//!   daemon up at install/boot time but does relaunch it after a crash.
//! * Logs in `~/Library/Logs/lazyagents/` (mode 0700) — never `/tmp`.
//!
//! Verbs are wired to `launchctl bootstrap / bootout / kickstart /
//! kill` — the modern `launchctl` API. The classic `load/unload` flow
//! is deliberately avoided because it bypasses session boundaries and
//! is brittle around SIP / "Headless and Login Item" rules in recent
//! macOS versions.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use super::actions::{ActionOutcome, ServiceVerb};
use super::paths::{InstallContext, LaunchdPaths};
use super::template::{render_template, xml_escape_text};
use super::{InstallError, InstallResult, ServiceController, ServiceMode};

pub const TEMPLATE: &str = include_str!("../../templates/dev.lazyagents.lad.plist");

/// `launchctl <verb> gui/<uid>` is the per-user domain. We resolve the
/// uid at runtime (effective uid) so a user with multiple accounts on
/// the same machine can install per-account.
fn current_uid() -> u32 {
    // libc::geteuid() on unix; on every other platform launchd never
    // applies anyway so the value is ignored by the rest of the code.
    #[cfg(unix)]
    {
        // SAFETY: geteuid is always safe; it has no side effects.
        unsafe { libc_geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(unix)]
extern "C" {
    fn geteuid() -> u32;
}

#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    geteuid()
}

#[derive(Debug, Clone)]
pub struct LaunchdController {
    pub paths: LaunchdPaths,
}

impl LaunchdController {
    pub fn from_ctx(ctx: &InstallContext) -> Self {
        Self {
            paths: LaunchdPaths::from_home(&ctx.home),
        }
    }

    pub fn rendered_plist(&self, ctx: &InstallContext) -> InstallResult<String> {
        // Every placeholder lands inside an XML `<string>` text node,
        // so the values must be XML-escaped. Otherwise a username or
        // path containing `&` / `<` / `>` (legal on the filesystem
        // and in AD-style logins like `CORP\foo&bar`) produces an
        // invalid plist that launchd silently rejects.
        let exec = xml_escape_text(&ctx.exec_path.to_string_lossy());
        let config = xml_escape_text(&ctx.config_path.to_string_lossy());
        let home = xml_escape_text(&ctx.home.to_string_lossy());
        let user = xml_escape_text(&ctx.user);
        let stdout = xml_escape_text(&self.paths.stdout_path.to_string_lossy());
        let stderr = xml_escape_text(&self.paths.stderr_path.to_string_lossy());
        let mut vars = BTreeMap::new();
        vars.insert("exec_path", exec.as_str());
        vars.insert("config_path", config.as_str());
        vars.insert("home_path", home.as_str());
        vars.insert("user", user.as_str());
        vars.insert("stdout_path", stdout.as_str());
        vars.insert("stderr_path", stderr.as_str());
        let rendered = render_template(TEMPLATE, &vars)?;
        // A3 belt-and-braces — refuse to write a plist that still
        // contains shell variables. A typo in the template or a future
        // edit that re-introduces `$HOME` should fail loudly here, not
        // produce a launchd unit that launchd silently rejects later.
        // We strip XML comments (<!-- ... -->) first so a docstring
        // mentioning the forbidden tokens by name doesn't trip the
        // guard.
        let body = strip_xml_comments(&rendered);
        for forbidden in ["~/", "$HOME", "$XDG_", "$USER"] {
            if body.contains(forbidden) {
                return Err(InstallError::Environment(format!(
                    "rendered plist still contains forbidden token `{forbidden}` — \
                     A3 requires absolute paths only"
                )));
            }
        }
        Ok(rendered)
    }

    fn service_target(&self) -> String {
        format!("gui/{}/{}", current_uid(), LaunchdPaths::LABEL)
    }

    fn domain_target(&self) -> String {
        format!("gui/{}", current_uid())
    }

    fn is_loaded(&self) -> bool {
        // `launchctl print <service-target>` returns 0 iff the service
        // is currently bootstrapped into the gui/<uid> domain.
        require_launchctl(&["print", &self.service_target()])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl ServiceController for LaunchdController {
    fn mode(&self) -> ServiceMode {
        ServiceMode::Launchd
    }

    fn install(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        let rendered = self.rendered_plist(ctx)?;
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Install,
                format!("would write {}", self.paths.plist_path.display()),
            ));
        }
        std::fs::create_dir_all(&self.paths.plist_dir)
            .map_err(|e| InstallError::io(&self.paths.plist_dir, e))?;
        ensure_logs_dir(&self.paths.logs_dir)?;
        let prior = if self.paths.plist_path.exists() {
            std::fs::read_to_string(&self.paths.plist_path).ok()
        } else {
            None
        };
        if prior.as_deref() == Some(rendered.as_str()) {
            return Ok(ActionOutcome::already(
                ServiceVerb::Install,
                format!("unchanged at {}", self.paths.plist_path.display()),
            ));
        }
        write_atomically(&self.paths.plist_path, rendered.as_bytes())?;
        Ok(ActionOutcome::done(
            ServiceVerb::Install,
            format!("wrote {}", self.paths.plist_path.display()),
        ))
    }

    fn enable(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Enable,
                format!(
                    "would run launchctl bootstrap {} {}",
                    self.domain_target(),
                    self.paths.plist_path.display()
                ),
            ));
        }
        if self.is_loaded() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Enable,
                format!(
                    "{} already bootstrapped into {}",
                    LaunchdPaths::LABEL,
                    self.domain_target()
                ),
            ));
        }
        let out = require_launchctl(&[
            "bootstrap",
            &self.domain_target(),
            &self.paths.plist_path.to_string_lossy(),
        ])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Enable,
                format!(
                    "launchctl bootstrap {} {}",
                    self.domain_target(),
                    self.paths.plist_path.display()
                ),
            ))
        } else {
            Err(InstallError::command(
                "launchctl bootstrap",
                out.stderr_string(),
            ))
        }
    }

    fn start(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Start,
                format!("would run launchctl kickstart -k {}", self.service_target()),
            ));
        }
        // A2 requires the three install verbs to be independently
        // composable: `lad install --service launchd --start` must work
        // even when the user did NOT pass `--enable`. launchctl
        // `kickstart` fails on a service that isn't bootstrapped into
        // the gui/<uid> domain, so we transparently bootstrap the
        // current plist first. This matches the implicit guarantee in
        // the post-install hint ("`lad install --service launchd
        // --start` to start now") and keeps `--enable` strictly
        // optional (it controls "load at next login", not "load now").
        if !self.is_loaded() && self.paths.plist_path.exists() {
            let out = require_launchctl(&[
                "bootstrap",
                &self.domain_target(),
                &self.paths.plist_path.to_string_lossy(),
            ])?;
            if !out.status.success() {
                return Err(InstallError::command(
                    "launchctl bootstrap (auto for --start)",
                    out.stderr_string(),
                ));
            }
        }
        let out = require_launchctl(&["kickstart", "-k", &self.service_target()])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Start,
                format!("launchctl kickstart -k {}", self.service_target()),
            ))
        } else {
            Err(InstallError::command(
                "launchctl kickstart",
                out.stderr_string(),
            ))
        }
    }

    fn stop(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Stop,
                format!("would run launchctl kill SIGTERM {}", self.service_target()),
            ));
        }
        let out = require_launchctl(&["kill", "SIGTERM", &self.service_target()])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Stop,
                format!("launchctl kill SIGTERM {}", self.service_target()),
            ))
        } else {
            // `launchctl kill` returns ESRCH when the service exists
            // but has no running process — that's the "already stopped"
            // case the A2 verb table expects to be idempotent.
            let stderr = out.stderr_string();
            if stderr.contains("Could not find") || stderr.contains("No such process") {
                Ok(ActionOutcome::already(
                    ServiceVerb::Stop,
                    format!("{} not running", LaunchdPaths::LABEL),
                ))
            } else {
                Err(InstallError::command("launchctl kill SIGTERM", stderr))
            }
        }
    }

    fn disable(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Disable,
                format!(
                    "would run launchctl bootout {} {}",
                    self.domain_target(),
                    self.paths.plist_path.display()
                ),
            ));
        }
        if !self.is_loaded() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Disable,
                format!(
                    "{} not bootstrapped in {}",
                    LaunchdPaths::LABEL,
                    self.domain_target()
                ),
            ));
        }
        let out = require_launchctl(&[
            "bootout",
            &self.domain_target(),
            &self.paths.plist_path.to_string_lossy(),
        ])?;
        if out.status.success() {
            Ok(ActionOutcome::done(
                ServiceVerb::Disable,
                format!(
                    "launchctl bootout {} {}",
                    self.domain_target(),
                    self.paths.plist_path.display()
                ),
            ))
        } else {
            Err(InstallError::command(
                "launchctl bootout",
                out.stderr_string(),
            ))
        }
    }

    fn uninstall(&self, ctx: &InstallContext) -> InstallResult<ActionOutcome> {
        if ctx.dry_run {
            return Ok(ActionOutcome::done(
                ServiceVerb::Uninstall,
                format!("would rm {}", self.paths.plist_path.display()),
            ));
        }
        if !self.paths.plist_path.exists() {
            return Ok(ActionOutcome::already(
                ServiceVerb::Uninstall,
                format!(
                    "no plist at {} (nothing to remove)",
                    self.paths.plist_path.display()
                ),
            ));
        }
        std::fs::remove_file(&self.paths.plist_path)
            .map_err(|e| InstallError::io(&self.paths.plist_path, e))?;
        Ok(ActionOutcome::done(
            ServiceVerb::Uninstall,
            format!("removed {}", self.paths.plist_path.display()),
        ))
    }
}

fn ensure_logs_dir(logs_dir: &Path) -> InstallResult<()> {
    std::fs::create_dir_all(logs_dir).map_err(|e| InstallError::io(logs_dir, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(logs_dir)
            .map_err(|e| InstallError::io(logs_dir, e))?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(logs_dir, perms).map_err(|e| InstallError::io(logs_dir, e))?;
    }
    Ok(())
}

/// Strip `<!-- ... -->` blocks so the A3 forbidden-token guard inside
/// `rendered_plist` doesn't fire on a docstring that mentions `$HOME`
/// or `~/` literally. Order-preserving and allocation-light; works on
/// the rendered template, not arbitrary user input.
fn strip_xml_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 < bytes.len() && &bytes[i..i + 4] == b"<!--" {
            if let Some(end) = s[i + 4..].find("-->") {
                i += 4 + end + 3;
                continue;
            }
            // Unterminated — bail out and copy the rest verbatim.
            out.push_str(&s[i..]);
            return out;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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
            .unwrap_or("lad-plist")
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
struct LaunchctlOutput {
    status: std::process::ExitStatus,
    #[allow(dead_code)]
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl LaunchctlOutput {
    fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

fn require_launchctl(args: &[&str]) -> InstallResult<LaunchctlOutput> {
    let mut cmd = Command::new("launchctl");
    for a in args {
        cmd.arg(a);
    }
    match cmd.output() {
        Ok(out) => Ok(LaunchctlOutput {
            status: out.status,
            stdout: out.stdout,
            stderr: out.stderr,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(InstallError::CommandMissing {
            command: "launchctl".to_string(),
        }),
        Err(e) => Err(InstallError::command("launchctl", e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> InstallContext {
        InstallContext {
            exec_path: PathBuf::from("/usr/local/bin/lad"),
            config_path: PathBuf::from("/Users/alice/.config/lazyagents/config.toml"),
            home: PathBuf::from("/Users/alice"),
            user: "alice".to_string(),
            dry_run: true,
        }
    }

    #[test]
    fn plist_has_absolute_exec_and_no_shell_vars() {
        let c = ctx();
        let ctrl = LaunchdController::from_ctx(&c);
        let rendered = ctrl.rendered_plist(&c).unwrap();
        // Absolute exec at index 0; absolute config follows --config.
        assert!(rendered.contains("<string>/usr/local/bin/lad</string>"));
        assert!(rendered.contains("<string>--config</string>"));
        assert!(rendered.contains("<string>/Users/alice/.config/lazyagents/config.toml</string>"));
        // Shell expansion tokens must not appear OUTSIDE comments —
        // the docstring block legitimately mentions `~/Library` and
        // friends so callers know what NOT to use. Strip comments
        // before asserting.
        let body = strip_xml_comments(&rendered);
        assert!(!body.contains("~/"), "no `~/` in body, body = {body}");
        assert!(!body.contains("$HOME"));
        assert!(!body.contains("$USER"));
        assert!(!body.contains("$XDG_"));
    }

    #[test]
    fn plist_sets_managed_by_and_pinned_path() {
        let c = ctx();
        let ctrl = LaunchdController::from_ctx(&c);
        let rendered = ctrl.rendered_plist(&c).unwrap();
        assert!(rendered.contains("<key>LAZYAGENTS_MANAGED_BY</key>"));
        assert!(rendered.contains("<string>launchd</string>"));
        assert!(rendered.contains("<key>LAZYAGENTS_CONFIG</key>"));
        // Hard rule: RunAtLoad=false, KeepAlive=true.
        assert!(rendered.contains("<key>RunAtLoad</key>"));
        assert!(rendered.contains("<false/>"));
        assert!(rendered.contains("<key>KeepAlive</key>"));
        assert!(rendered.contains("<true/>"));
        // Logs path lives under ~/Library/Logs/lazyagents, NOT /tmp.
        // The path is derived via `home.join("Library").join("Logs")...`,
        // which on Windows substitutes `\` for the separator it adds —
        // and the rendered XML carries that exact string. Accept both
        // separators so the test stays valid in the matrix CI.
        let has_out_log = rendered.contains("/Users/alice/Library/Logs/lazyagents/lad.out.log")
            || rendered.contains("/Users/alice\\Library\\Logs\\lazyagents\\lad.out.log");
        let has_err_log = rendered.contains("/Users/alice/Library/Logs/lazyagents/lad.err.log")
            || rendered.contains("/Users/alice\\Library\\Logs\\lazyagents\\lad.err.log");
        assert!(
            has_out_log,
            "stdout path missing in rendered plist: {rendered}"
        );
        assert!(
            has_err_log,
            "stderr path missing in rendered plist: {rendered}"
        );
        assert!(!rendered.contains("/tmp/"));
        // PATH minimal set; HOME/USER explicit.
        assert!(rendered.contains("/usr/bin:/bin:/usr/sbin:/sbin"));
    }

    #[test]
    fn plist_xml_escapes_metacharacters_in_paths_and_user() {
        // Real-world paths can contain `&` (think "C:\Users\foo & bar"
        // or `/Users/Alice & Bob`); usernames in mixed environments
        // can contain `<` / `>` / quotes. Unescaped, these would
        // produce invalid plist XML that launchd rejects at bootstrap.
        let mut c = ctx();
        c.exec_path = PathBuf::from("/opt/Acme & Co/lad");
        c.config_path = PathBuf::from("/Users/al<ice/cfg.toml");
        c.user = "al'ice & \"bob\"".to_string();
        let ctrl = LaunchdController::from_ctx(&c);
        let rendered = ctrl.rendered_plist(&c).unwrap();
        // Raw metacharacters must NOT appear in <string> values; their
        // escaped forms must.
        assert!(
            rendered.contains("<string>/opt/Acme &amp; Co/lad</string>"),
            "exec path not escaped: {rendered}"
        );
        assert!(
            rendered.contains("<string>/Users/al&lt;ice/cfg.toml</string>"),
            "config path not escaped: {rendered}"
        );
        assert!(
            rendered.contains("<string>al&apos;ice &amp; &quot;bob&quot;</string>"),
            "user not escaped: {rendered}"
        );
        // And the doc must still be well-formed enough that an XML
        // parser can read every element; quickest local check is that
        // we have a single balanced root <plist>.
        let plist_open = rendered.matches("<plist").count();
        let plist_close = rendered.matches("</plist>").count();
        assert_eq!(plist_open, 1);
        assert_eq!(plist_close, 1);
    }

    #[test]
    fn start_dry_run_message_unchanged() {
        // A2 hard rule: `lad install --service launchd --start` is a
        // valid orthogonal combo even without `--enable`. The dry-run
        // output stays focused on the kickstart command — the
        // auto-bootstrap fallback is a real-mode behavior, gated on
        // `!is_loaded() && plist_path.exists()` and never shells out in
        // dry-run.
        let c = ctx();
        let ctrl = LaunchdController::from_ctx(&c);
        let outcome = ctrl.start(&c).expect("dry-run start");
        let s = outcome.to_string();
        assert!(s.contains("launchctl kickstart -k"), "{s}");
    }
}
