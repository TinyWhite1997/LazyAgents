//! Cross-platform `resolve_config_path()` — the single source of truth
//! for "where does LazyAgents look for `config.toml`?".
//!
//! Used by `lad config path`, by future `lad install --service launchd`
//! (writing the resolved absolute path into the plist), and by the
//! daemon's bootstrap when it has to materialise a default config file.
//! Keeping the chain in one function avoids three subtly-different
//! lookups drifting apart across the codebase — addendum A4 hard rule.

use std::path::PathBuf;

use crate::ENV_CONFIG;

/// Outcome of [`resolve_config_path`]. We always return *some* path so
/// the caller can both *read* (`existing`) and *write* (`write_target`)
/// without re-running the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfigPath {
    /// First chain entry that exists on disk. `None` = no file found.
    pub existing: Option<PathBuf>,
    /// Path the daemon should write to if it needs to materialise a
    /// fresh default config. Always populated; on macOS still resolves
    /// to `~/.config/lazyagents/config.toml` (third chain entry), NOT
    /// `~/Library/Application Support/lazyagents/config.toml`. Rationale
    /// (A4): the macOS-only entry is a *read* compatibility step so a
    /// user who copied their config from `Application Support` continues
    /// to work; writes always go to the XDG-style location so a single
    /// dotfile sync covers Linux and macOS.
    pub write_target: PathBuf,
    /// Full ordered list the chain walked, for `lad config path -v` /
    /// future diagnostics. Always non-empty.
    pub chain: Vec<PathBuf>,
}

impl ResolvedConfigPath {
    /// Convenience for `lad config path` — prefer the existing file,
    /// otherwise the write target.
    pub fn primary(&self) -> &PathBuf {
        self.existing.as_ref().unwrap_or(&self.write_target)
    }
}

/// Resolve the config path using process env + real filesystem checks.
pub fn resolve_config_path() -> ResolvedConfigPath {
    resolve_with(EnvLookup::process(), |p| p.exists())
}

/// Path the M4.1 `lad install --service *` writer should bake into
/// service unit files (launchd plist, systemd `--config`, Windows task
/// XML). Returns the *existing* file if one exists today; otherwise the
/// write target. Always an absolute path on platforms where `$HOME` /
/// `%APPDATA%` resolves; falls back to the relative chain head when
/// neither is set (degraded sandbox).
pub fn config_path_for_install() -> PathBuf {
    let resolved = resolve_config_path();
    resolved.primary().clone()
}

/// Pluggable env lookup so tests can drive `resolve_with` without
/// mutating process-global env vars (which races other tests in the
/// same binary).
#[derive(Debug, Clone, Default)]
pub struct EnvLookup {
    /// Snapshot of the relevant env vars (`None` = unset / empty).
    pub lazyagents_config: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`
    pub xdg_config_home: Option<PathBuf>,
    /// `$HOME` (POSIX)
    pub home: Option<PathBuf>,
    /// `%APPDATA%` (Windows)
    pub appdata: Option<PathBuf>,
}

impl EnvLookup {
    /// Snapshot the current process env.
    pub fn process() -> Self {
        Self {
            lazyagents_config: env_path(ENV_CONFIG),
            xdg_config_home: env_path("XDG_CONFIG_HOME"),
            home: env_path("HOME"),
            appdata: env_path("APPDATA"),
        }
    }
}

/// Test-injectable variant of [`resolve_config_path`].
///
/// `exists` is the predicate that decides whether a candidate is the
/// "first existing" file — in tests, callers pass a closure that
/// consults a tempdir; in production, `std::path::Path::exists`.
pub fn resolve_with(
    env: EnvLookup,
    exists: impl Fn(&std::path::Path) -> bool,
) -> ResolvedConfigPath {
    let chain = build_chain(&env);

    let existing = chain.iter().find(|p| exists(p)).cloned();
    let write_target = default_write_target(&env);

    ResolvedConfigPath {
        existing,
        write_target,
        chain,
    }
}

/// Build the candidate chain. Pure function — no FS access.
fn build_chain(env: &EnvLookup) -> Vec<PathBuf> {
    let mut chain = Vec::with_capacity(4);

    // 1. Explicit env override wins on every platform.
    if let Some(p) = &env.lazyagents_config {
        chain.push(p.clone());
    }

    if cfg!(windows) {
        // Windows runs an independent chain: `%APPDATA%\lazyagents\config.toml`.
        // (A4 explicitly notes Windows is out of scope for the macOS chain.)
        if let Some(appdata) = &env.appdata {
            chain.push(appdata.join("lazyagents").join("config.toml"));
        } else if let Some(home) = &env.home {
            // Degraded WSL / mingw env without %APPDATA% — fall back
            // to the same XDG layout the Linux build uses so tests in
            // a Windows-sandboxed CI runner still resolve a sane path.
            chain.push(home.join(".config").join("lazyagents").join("config.toml"));
        }
        return chain;
    }

    // POSIX chain (Linux + macOS share the first two steps).
    if let Some(xdg) = &env.xdg_config_home {
        chain.push(xdg.join("lazyagents").join("config.toml"));
    }
    if let Some(home) = &env.home {
        chain.push(home.join(".config").join("lazyagents").join("config.toml"));
    }

    // macOS-only compatibility fallback for users whose tooling drops
    // configs into Apple's Application Support directory.
    if cfg!(target_os = "macos") {
        if let Some(home) = &env.home {
            chain.push(
                home.join("Library")
                    .join("Application Support")
                    .join("lazyagents")
                    .join("config.toml"),
            );
        }
    }

    chain
}

/// The path daemon bootstrap should write a fresh default config to.
/// Matches the third chain entry on POSIX (XDG-style under `$HOME`) and
/// `%APPDATA%\lazyagents\config.toml` on Windows. Never returns the
/// macOS-only step — see `ResolvedConfigPath::write_target` docs.
fn default_write_target(env: &EnvLookup) -> PathBuf {
    if cfg!(windows) {
        if let Some(appdata) = &env.appdata {
            return appdata.join("lazyagents").join("config.toml");
        }
    }
    if let Some(xdg) = &env.xdg_config_home {
        return xdg.join("lazyagents").join("config.toml");
    }
    if let Some(home) = &env.home {
        return home.join(".config").join("lazyagents").join("config.toml");
    }
    // Last-resort literal — extremely degraded env (no HOME, no XDG,
    // no APPDATA). The daemon will surface the resulting permission
    // error when it tries to write; better than silently picking `/`.
    PathBuf::from(".config/lazyagents/config.toml")
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_exists(_: &std::path::Path) -> bool {
        false
    }

    fn always_exists(_: &std::path::Path) -> bool {
        true
    }

    #[test]
    fn env_override_wins_over_xdg_and_home() {
        let env = EnvLookup {
            lazyagents_config: Some(PathBuf::from("/explicit/cfg.toml")),
            xdg_config_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
            appdata: None,
        };
        let chain = build_chain(&env);
        assert_eq!(chain[0], PathBuf::from("/explicit/cfg.toml"));
    }

    #[test]
    #[cfg(not(windows))]
    fn posix_chain_order_xdg_then_home() {
        let env = EnvLookup {
            lazyagents_config: None,
            xdg_config_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
            appdata: None,
        };
        let chain = build_chain(&env);
        assert!(chain[0].starts_with("/xdg"));
        assert!(chain[1].starts_with("/home/u/.config"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_chain_ends_with_application_support() {
        let env = EnvLookup {
            lazyagents_config: None,
            xdg_config_home: None,
            home: Some(PathBuf::from("/Users/u")),
            appdata: None,
        };
        let chain = build_chain(&env);
        assert_eq!(chain.len(), 2);
        assert_eq!(
            chain[0],
            PathBuf::from("/Users/u/.config/lazyagents/config.toml")
        );
        assert_eq!(
            chain[1],
            PathBuf::from("/Users/u/Library/Application Support/lazyagents/config.toml")
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn non_macos_skips_application_support_step() {
        let env = EnvLookup {
            lazyagents_config: None,
            xdg_config_home: None,
            home: Some(PathBuf::from("/home/u")),
            appdata: None,
        };
        let chain = build_chain(&env);
        for p in &chain {
            assert!(
                !p.to_string_lossy().contains("Application Support"),
                "non-macos chain leaked Application Support: {chain:?}"
            );
        }
    }

    #[test]
    fn write_target_falls_back_through_xdg_home_literal() {
        let xdg = EnvLookup {
            lazyagents_config: None,
            xdg_config_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
            appdata: None,
        };
        assert_eq!(
            default_write_target(&xdg),
            PathBuf::from("/xdg/lazyagents/config.toml")
        );

        let home_only = EnvLookup {
            lazyagents_config: None,
            xdg_config_home: None,
            home: Some(PathBuf::from("/home/u")),
            appdata: None,
        };
        assert_eq!(
            default_write_target(&home_only),
            PathBuf::from("/home/u/.config/lazyagents/config.toml")
        );

        let degraded = EnvLookup::default();
        assert_eq!(
            default_write_target(&degraded),
            PathBuf::from(".config/lazyagents/config.toml")
        );
    }

    #[test]
    fn existing_returns_first_match_in_chain_order() {
        let env = EnvLookup {
            lazyagents_config: None,
            xdg_config_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
            appdata: None,
        };
        // Only the second candidate exists — make sure we picked it.
        let resolved = resolve_with(env.clone(), |p| {
            p == std::path::Path::new("/home/u/.config/lazyagents/config.toml")
        });
        assert_eq!(
            resolved.existing.as_deref(),
            Some(std::path::Path::new(
                "/home/u/.config/lazyagents/config.toml"
            ))
        );

        let none = resolve_with(env, never_exists);
        assert!(none.existing.is_none());

        let env2 = EnvLookup {
            lazyagents_config: Some(PathBuf::from("/explicit/x.toml")),
            ..EnvLookup::default()
        };
        let any = resolve_with(env2, always_exists);
        assert_eq!(
            any.existing.as_deref(),
            Some(std::path::Path::new("/explicit/x.toml"))
        );
    }
}
