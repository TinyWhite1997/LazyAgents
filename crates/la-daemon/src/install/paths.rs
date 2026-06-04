//! Path + identity resolution shared by every controller.
//!
//! Three concerns live here:
//!
//! 1. **Where do unit files go?** Each [`SystemdPaths`] / [`LaunchdPaths`]
//!    / [`WindowsTaskPaths`] resolves the canonical install location
//!    from `$HOME` / `%APPDATA%` + the static template filename.
//! 2. **What absolute exec path do we bake into the unit?** A3 requires
//!    `which lad` first, falling back to `env::current_exe()` — that
//!    rule lives in [`resolve_exec_path`] so the launchd + systemd +
//!    Windows writers all see the same answer.
//! 3. **Who is the running user?** `resolve_home` + `resolve_user`
//!    snapshot `$HOME` / `$USER` (or `%USERPROFILE%` / `%USERNAME%`)
//!    so the launchd plist + Windows task XML can embed them as
//!    absolute strings.

use std::path::{Path, PathBuf};

/// All the pieces a [`ServiceController`] needs to render its template
/// and execute its primitive verbs. Built once at the top of `lad
/// install` / `lad uninstall` and passed through unchanged.
///
/// [`ServiceController`]: crate::install::ServiceController
#[derive(Debug, Clone)]
pub struct InstallContext {
    /// Absolute path to the `lad` (or `lad.exe`) binary.
    pub exec_path: PathBuf,
    /// Absolute path to the config file that should be passed via
    /// `lad start --config <path>`. Resolved by
    /// [`la_config::resolve_config_path`] (or `--config` override).
    pub config_path: PathBuf,
    /// Resolved `$HOME` / `%USERPROFILE%`.
    pub home: PathBuf,
    /// Resolved `$USER` / `%USERNAME%`.
    pub user: String,
    /// Whether `lad install/uninstall` was invoked in dry-run mode.
    /// Controllers must honour this — write nothing to disk, run
    /// no `systemctl/launchctl/schtasks` command — and still report
    /// realistic [`crate::install::ActionOutcome::Done`] entries so
    /// the CLI shows what *would* happen.
    pub dry_run: bool,
}

/// Resolve the absolute exec path the unit should bake in.
///
/// A3 contract: `which lad` first (handles upgrades-in-place against a
/// PATH entry like Homebrew/`~/.cargo/bin`), then fall back to
/// `env::current_exe()`. A relative path is never acceptable — launchd
/// in particular does not search `$PATH`.
pub fn resolve_exec_path(binary_name: &str) -> std::io::Result<PathBuf> {
    if let Some(found) = which_in_path(binary_name) {
        return Ok(found);
    }
    let exe = std::env::current_exe()?;
    Ok(exe)
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe_candidate = dir.join(format!("{name}.exe"));
            if exe_candidate.is_file() {
                return Some(exe_candidate);
            }
        }
    }
    None
}

/// Resolve `$HOME` / `%USERPROFILE%`. Errors if neither is set — every
/// service template needs a real path here (no shell variables, A3).
pub fn resolve_home() -> std::io::Result<PathBuf> {
    if let Some(v) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    if let Some(v) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "neither $HOME nor %USERPROFILE% set",
    ))
}

/// Resolve `$USER` / `%USERNAME%`. Errors if neither is set.
pub fn resolve_user() -> std::io::Result<String> {
    for var in ["USER", "LOGNAME", "USERNAME"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            if let Ok(s) = v.into_string() {
                if !s.is_empty() {
                    return Ok(s);
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "neither $USER nor %USERNAME% set",
    ))
}

// ---------------- systemd ----------------

/// Filenames + directories for the systemd user unit.
#[derive(Debug, Clone)]
pub struct SystemdPaths {
    pub unit_dir: PathBuf,
    pub unit_path: PathBuf,
}

impl SystemdPaths {
    pub const UNIT_NAME: &'static str = "lad.service";

    /// Locate `~/.config/systemd/user/lad.service`. `$XDG_CONFIG_HOME`
    /// is honoured if set (systemd itself reads from it).
    pub fn from_home(home: &Path) -> Self {
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let unit_dir = xdg.join("systemd").join("user");
        let unit_path = unit_dir.join(Self::UNIT_NAME);
        Self {
            unit_dir,
            unit_path,
        }
    }
}

// ---------------- launchd ----------------

/// Filenames + directories for the launchd user agent (A3).
#[derive(Debug, Clone)]
pub struct LaunchdPaths {
    pub label: String,
    pub plist_dir: PathBuf,
    pub plist_path: PathBuf,
    pub logs_dir: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

impl LaunchdPaths {
    pub const LABEL: &'static str = "dev.lazyagents.lad";
    pub const PLIST_FILENAME: &'static str = "dev.lazyagents.lad.plist";

    pub fn from_home(home: &Path) -> Self {
        let plist_dir = home.join("Library").join("LaunchAgents");
        let plist_path = plist_dir.join(Self::PLIST_FILENAME);
        let logs_dir = home.join("Library").join("Logs").join("lazyagents");
        let stdout_path = logs_dir.join("lad.out.log");
        let stderr_path = logs_dir.join("lad.err.log");
        Self {
            label: Self::LABEL.to_string(),
            plist_dir,
            plist_path,
            logs_dir,
            stdout_path,
            stderr_path,
        }
    }
}

// ---------------- Windows Scheduled Task ----------------

/// Filenames + identifiers for the Windows Scheduled Task (A8).
#[derive(Debug, Clone)]
pub struct WindowsTaskPaths {
    /// `\LazyAgents\lad` — fully qualified Task Scheduler name.
    pub task_name: String,
    /// `%APPDATA%\lazyagents\lad-task.xml` — where we cache the XML so
    /// the install/uninstall verbs can re-import on demand.
    pub xml_dir: PathBuf,
    pub xml_path: PathBuf,
    /// Working directory baked into the task. Always the agent's
    /// `%APPDATA%\lazyagents` so logs land next to the SQLite state.
    pub working_dir: PathBuf,
}

impl WindowsTaskPaths {
    pub const TASK_NAME: &'static str = r"\LazyAgents\lad";
    pub const XML_FILENAME: &'static str = "lad-task.xml";

    /// Resolve from `%APPDATA%` (preferred) or, in degraded WSL/mingw
    /// runners that don't set it, `$HOME/.config/lazyagents`.
    pub fn from_appdata_or_home(appdata: Option<&Path>, home: &Path) -> Self {
        let base = appdata
            .map(|p| p.join("lazyagents"))
            .unwrap_or_else(|| home.join(".config").join("lazyagents"));
        Self {
            task_name: Self::TASK_NAME.to_string(),
            xml_dir: base.clone(),
            xml_path: base.join(Self::XML_FILENAME),
            working_dir: base,
        }
    }
}

/// Cheap detector for "is some service manager already supervising
/// the daemon". Used by `lad start` to refuse to launch a second copy.
///
/// Returns the wire tag of the supervisor (`"systemd"` / `"launchd"` /
/// `"windows-task"`) if set, else `None`. The function is intentionally
/// trivial — we trust `LAZYAGENTS_MANAGED_BY` because we set it in the
/// service unit ourselves; we don't try to scan `systemctl list-units`
/// or `launchctl print` (slow + permission-prone).
pub fn detect_running_service() -> Option<String> {
    std::env::var("LAZYAGENTS_MANAGED_BY")
        .ok()
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_path_uses_xdg() {
        // Don't poke real env vars — exercise the path-building only.
        let p = SystemdPaths::from_home(Path::new("/home/u"));
        // With or without XDG_CONFIG_HOME, the unit name is stable.
        assert!(p.unit_path.ends_with("lad.service"));
        assert!(p.unit_dir.ends_with("systemd/user"));
    }

    #[test]
    fn launchd_paths_anchor_at_home_library() {
        let p = LaunchdPaths::from_home(Path::new("/Users/alice"));
        assert_eq!(
            p.plist_path,
            PathBuf::from("/Users/alice/Library/LaunchAgents/dev.lazyagents.lad.plist")
        );
        assert_eq!(
            p.stdout_path,
            PathBuf::from("/Users/alice/Library/Logs/lazyagents/lad.out.log")
        );
        assert_eq!(p.label, "dev.lazyagents.lad");
    }

    #[test]
    fn windows_task_paths_prefer_appdata() {
        let with_appdata = WindowsTaskPaths::from_appdata_or_home(
            Some(Path::new(r"C:\Users\bob\AppData\Roaming")),
            Path::new(r"C:\Users\bob"),
        );
        assert!(
            with_appdata
                .xml_path
                .to_string_lossy()
                .ends_with("lazyagents/lad-task.xml")
                || with_appdata
                    .xml_path
                    .to_string_lossy()
                    .ends_with(r"lazyagents\lad-task.xml")
        );

        let degraded = WindowsTaskPaths::from_appdata_or_home(None, Path::new("/home/u"));
        assert!(degraded
            .xml_path
            .to_string_lossy()
            .contains(".config/lazyagents"));
    }
}
