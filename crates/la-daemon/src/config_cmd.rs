//! `lad config` subcommand implementation (WEK-71 / M4.2).
//!
//! Three verbs:
//!
//! - `lad config show` — print the merged configuration with provenance
//!   comments (`# from CLI flag / env / config / default`).
//! - `lad config check [--config <path>]` — parse + validate the file
//!   without starting the daemon; exit non-zero on schema errors so CI
//!   can gate on it.
//! - `lad config path` — print the file `resolve_config_path()` would
//!   read (the existing one if any, otherwise the default write target).
//!
//! Kept in the library (not in `src/bin/lad.rs`) so the integration
//! suite can call it without shelling out, and so the test for the
//! `LAZYAGENTS_LOG` deprecation warning can exercise the same code path
//! `lad` does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use la_config::{
    resolve_config_path, source, AdapterConfig, ClaudeConfig, CodexConfig, Config, ConfigError,
    LogFormat, LogLevel, OpencodeConfig, Source, ENV_CONFIG, ENV_LOG_FORMAT, ENV_LOG_LEVEL,
    ENV_LOG_LEVEL_DEPRECATED, ENV_SOCKET, ENV_STATE_DIR,
};

/// Reasons `lad config check` rejects a file. Carries the path so the
/// caller can render `lad: config check failed at /etc/lazyagents/config.toml: ...`.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// Schema parse — typo, wrong type, unknown field (deny_unknown_fields).
    #[error("schema: {0}")]
    Schema(#[from] ConfigError),
    /// Cross-field validation failure (e.g. `listen_tcp` non-empty).
    #[error("{0}")]
    Validation(String),
}

/// CLI overrides forwarded from the `lad` binary. Mirrors the M1.7
/// flags so the precedence test in `show` proves `--socket` beats
/// `LAZYAGENTS_SOCKET` beats `[daemon].socket_path` beats default.
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    /// `--socket`
    pub socket: Option<PathBuf>,
    /// `--state-dir`
    pub state_dir: Option<PathBuf>,
    /// `--log-level`
    pub log_level: Option<String>,
    /// `--log-format`
    pub log_format: Option<String>,
    /// `--config`
    pub config_path: Option<PathBuf>,
}

/// `lad config show` — load the file (if any) + env + CLI, render the
/// merged result as TOML with a trailing `# from …` comment per line.
///
/// Returns the rendered string instead of writing to stdout so tests can
/// snapshot it. Stdout is the caller's job.
pub fn show(overrides: &CliOverrides) -> Result<String, CheckError> {
    let chain = resolve_config_path();
    let config_path = overrides
        .config_path
        .clone()
        .or_else(|| chain.existing.clone());

    let file_cfg = match config_path.as_deref() {
        Some(p) if p.exists() => Some(Config::load_file(p)?),
        _ => None,
    };
    if let Some(cfg) = file_cfg.as_ref() {
        validate(cfg)?;
    }
    let env = EnvSnapshot::from_process();
    validate_cli_env_enums(overrides, &env)?;

    Ok(render(
        overrides,
        &env,
        file_cfg.as_ref(),
        config_path.as_deref(),
    ))
}

/// `lad config check` — schema + cross-field validation against the
/// given path (defaults to `resolve_config_path()`'s primary).
///
/// `explicit` distinguishes how the path was chosen:
///
/// - `false` — caller did not pass `--config` / `LAZYAGENTS_CONFIG`;
///   resolver picked the path. A missing file on disk here is fine
///   (a fresh checkout with no user config falls back to defaults),
///   so we return `Ok(existed=false)` and let the caller exit 0.
/// - `true` — caller named a specific file; if it's not on disk that
///   IS a failure. The A1/M4.0.5 CI hook runs
///   `lad config check --config templates/config.example.toml`; if the
///   template ever gets deleted, renamed, or the CI runner's cwd
///   shifts, exit-0-on-missing would let the gate silently pass and
///   stop proving the example file exists + parses. Returning
///   `CheckError::Validation` here forces the gate to fail loudly.
///
/// `deny_unknown_fields` is enforced by the schema structs themselves
/// — a typo surfaces as a [`CheckError::Schema`] carrying the
/// offending field name in toml's human-readable message
/// ("unknown field `sokcet_path`, expected one of …").
pub fn check(path: Option<&Path>, explicit: bool) -> Result<CheckSummary, CheckError> {
    let resolved = resolve_config_path();
    let target = path
        .map(Path::to_path_buf)
        .or_else(|| resolved.existing.clone())
        .unwrap_or_else(|| resolved.write_target.clone());

    if !target.exists() {
        if explicit {
            return Err(CheckError::Validation(format!(
                "config file {} does not exist (explicit --config / LAZYAGENTS_CONFIG)",
                target.display()
            )));
        }
        // Missing-file under the resolver-default path is *not* a
        // parse error; report it as a no-op success so CI on a fresh
        // checkout (where the user has not shipped a config) does
        // not block the pipeline.
        return Ok(CheckSummary {
            path: target,
            existed: false,
            sections_present: Vec::new(),
        });
    }

    let raw = std::fs::read_to_string(&target).map_err(|e| CheckError::Schema(e.into()))?;
    let parsed = Config::parse_str(&raw)?;
    validate(&parsed)?;
    Ok(CheckSummary {
        path: target,
        existed: true,
        sections_present: top_level_sections(&raw),
    })
}

/// Pure-function `check` variant for tests that already hold the file
/// contents in memory.
pub fn check_str(raw: &str) -> Result<Config, CheckError> {
    let parsed = Config::parse_str(raw)?;
    validate(&parsed)?;
    Ok(parsed)
}

/// `lad config path` — print the path the resolver would consult.
pub fn path_string() -> String {
    let resolved = resolve_config_path();
    resolved.primary().display().to_string()
}

/// Process-wide env snapshot used by `show` and `start`.
#[derive(Debug, Default, Clone)]
pub struct EnvSnapshot {
    /// `LAZYAGENTS_SOCKET`
    pub socket: Option<PathBuf>,
    /// `LAZYAGENTS_STATE_DIR`
    pub state_dir: Option<PathBuf>,
    /// `LAZYAGENTS_LOG_LEVEL` (or deprecated `LAZYAGENTS_LOG`)
    pub log_level: Option<String>,
    /// `LAZYAGENTS_LOG_FORMAT`
    pub log_format: Option<String>,
    /// `LAZYAGENTS_CONFIG`
    pub config: Option<PathBuf>,
}

impl EnvSnapshot {
    /// Read the current process env. Emits the `LAZYAGENTS_LOG` →
    /// `LAZYAGENTS_LOG_LEVEL` deprecation warning at most once per
    /// process via tracing (and as a stderr fallback if no subscriber
    /// is registered yet — which is the case during early bootstrap).
    pub fn from_process() -> Self {
        let socket = env_path(ENV_SOCKET);
        let state_dir = env_path(ENV_STATE_DIR);
        let log_level = env_string(ENV_LOG_LEVEL).or_else(|| {
            let alias = env_string(ENV_LOG_LEVEL_DEPRECATED);
            if alias.is_some() {
                emit_log_deprecation_warning();
            }
            alias
        });
        let log_format = env_string(ENV_LOG_FORMAT);
        let config = env_path(ENV_CONFIG);
        Self {
            socket,
            state_dir,
            log_level,
            log_format,
            config,
        }
    }
}

/// Final resolved daemon-shaped values + their provenance. The caller
/// (`run_foreground` in `lad.rs`) uses this to drive `Daemon::bind` and
/// `init_observability` instead of reaching back into env / file.
#[derive(Debug, Clone)]
pub struct ResolvedDaemonValues {
    /// `--socket / LAZYAGENTS_SOCKET / [daemon].socket_path / default`
    pub socket_override: Option<PathBuf>,
    /// Provenance of `socket_override`.
    pub socket_source: Source,
    /// `--state-dir / LAZYAGENTS_STATE_DIR / [daemon].state_dir / default`
    pub state_dir_override: Option<PathBuf>,
    /// Provenance of `state_dir_override`.
    pub state_dir_source: Source,
    /// `--log-level / LAZYAGENTS_LOG_LEVEL / [daemon].log_level / default`
    pub log_level: LogLevel,
    /// Provenance of `log_level`.
    pub log_level_source: Source,
    /// `--log-format / LAZYAGENTS_LOG_FORMAT / [daemon].log_format / default`
    pub log_format: LogFormat,
    /// Provenance of `log_format`.
    pub log_format_source: Source,
}

/// Apply the four-tier precedence to derive the values `lad start` will
/// actually use. Pure function — tests can drive it without touching
/// the process env. `file` is `None` when there is no config file on
/// disk (so the file layer never contributes a source).
pub fn resolve_daemon_values(
    overrides: &CliOverrides,
    env: &EnvSnapshot,
    file: Option<&Config>,
) -> ResolvedDaemonValues {
    let file_daemon = file.map(|c| &c.daemon);
    let (socket_override, socket_source) = resolve_optional(
        overrides.socket.clone(),
        env.socket.clone(),
        file_daemon.and_then(|d| d.socket_path.clone()),
    );
    let (state_dir_override, state_dir_source) = resolve_optional(
        overrides.state_dir.clone(),
        env.state_dir.clone(),
        file_daemon.and_then(|d| d.state_dir.clone()),
    );
    let (log_level, log_level_source) = source::resolve(
        overrides
            .log_level
            .as_deref()
            .and_then(LogLevel::parse_label),
        env.log_level.as_deref().and_then(LogLevel::parse_label),
        file_daemon.map(|d| d.log_level),
        LogLevel::default(),
    );
    let (log_format, log_format_source) = source::resolve(
        overrides
            .log_format
            .as_deref()
            .and_then(LogFormat::parse_label),
        env.log_format.as_deref().and_then(LogFormat::parse_label),
        file_daemon.map(|d| d.log_format),
        LogFormat::default(),
    );

    ResolvedDaemonValues {
        socket_override,
        socket_source,
        state_dir_override,
        state_dir_source,
        log_level,
        log_level_source,
        log_format,
        log_format_source,
    }
}

/// Variant of [`source::resolve`] for values whose "default" is `None`
/// (i.e. the daemon falls back to its own path-discovery routine when
/// neither CLI nor env nor file set them).
fn resolve_optional<T: Clone>(
    cli: Option<T>,
    env: Option<T>,
    file: Option<T>,
) -> (Option<T>, Source) {
    if let Some(v) = cli {
        (Some(v), Source::Cli)
    } else if let Some(v) = env {
        (Some(v), Source::Env)
    } else if let Some(v) = file {
        (Some(v), Source::ConfigFile)
    } else {
        (None, Source::Default)
    }
}

/// Summary returned by [`check`] so the caller can print a friendly
/// success line ("OK: 4 sections present at /home/u/...").
#[derive(Debug, Clone)]
pub struct CheckSummary {
    /// The file that was actually checked.
    pub path: PathBuf,
    /// `false` = nothing to check (missing file is not a failure).
    pub existed: bool,
    /// Top-level `[section]` names found in the raw text. Best-effort
    /// (regex-free, just `^[`-prefix scan) — used only for the success
    /// banner so a wrong list does not cause a false negative.
    pub sections_present: Vec<String>,
}

/// Reject invalid enum values supplied via CLI flags or env vars before
/// they get silently demoted to `None` by `parse_label`. Without this
/// guard a `--log-level typo` would fall through to the file/default
/// layer and `lad config show` would mis-report the value as `# from
/// default`, contradicting the documented "CLI > env > file > default"
/// precedence. File-layer values cannot reach here: serde already
/// validated the enum at parse time, so any TOML typo errored in
/// [`Config::parse_str`].
pub fn validate_cli_env_enums(
    overrides: &CliOverrides,
    env: &EnvSnapshot,
) -> Result<(), CheckError> {
    check_enum(
        "--log-level",
        overrides.log_level.as_deref(),
        LogLevel::parse_label,
    )?;
    check_enum(
        "LAZYAGENTS_LOG_LEVEL",
        env.log_level.as_deref(),
        LogLevel::parse_label,
    )?;
    check_enum(
        "--log-format",
        overrides.log_format.as_deref(),
        LogFormat::parse_label,
    )?;
    check_enum(
        "LAZYAGENTS_LOG_FORMAT",
        env.log_format.as_deref(),
        LogFormat::parse_label,
    )?;
    Ok(())
}

fn check_enum<T>(
    source_label: &str,
    raw: Option<&str>,
    parse: fn(&str) -> Option<T>,
) -> Result<(), CheckError> {
    let Some(v) = raw else {
        return Ok(());
    };
    if parse(v).is_some() {
        return Ok(());
    }
    Err(CheckError::Validation(format!(
        "{source_label} value {:?} is not recognised",
        v
    )))
}

/// Cross-field validation shared by `lad config check` and every
/// runtime path that loads the config (`lad start / daemonize / metrics
/// / doctor / backup`). Keeping the rule set in one place means a
/// future `[daemon].listen_tcp = "..."` cannot pass `check` but get
/// silently ignored by `start` — the reviewer-flagged
/// "configured but nothing happened" hole.
pub fn validate(cfg: &Config) -> Result<(), CheckError> {
    validate_impl(cfg)
}

fn validate_impl(cfg: &Config) -> Result<(), CheckError> {
    if !cfg.daemon.listen_tcp.is_empty() {
        return Err(CheckError::Validation(format!(
            "[daemon].listen_tcp is set to {:?} but TCP listener is not supported in v1 (architecture §10.1). Leave the field empty.",
            cfg.daemon.listen_tcp
        )));
    }
    if cfg.scheduler.global_max_concurrent_runs == 0 {
        return Err(CheckError::Validation(
            "[scheduler].global_max_concurrent_runs must be >= 1".to_string(),
        ));
    }
    // `theme` may name a built-in palette or any `[[ui.custom_theme]]`
    // defined below. The daemon doesn't render — it only sanity-checks
    // that the id resolves to *something*, so a typo surfaces at `config
    // check` instead of silently demoting to `auto` in the TUI.
    const BUILTIN_THEME_IDS: &[&str] = &[
        "auto",
        "dark",
        "light",
        "catppuccin-latte",
        "catppuccin-frappe",
        "catppuccin-macchiato",
        "catppuccin-mocha",
        "gruvbox-dark",
        "gruvbox-light",
        "nord",
        "dracula",
        "tokyo-night",
        "solarized-dark",
        "solarized-light",
    ];
    let theme_known = BUILTIN_THEME_IDS.contains(&cfg.ui.theme.as_str())
        || cfg.ui.custom_theme.iter().any(|c| c.id == cfg.ui.theme);
    if !theme_known {
        return Err(CheckError::Validation(format!(
            "[ui].theme {:?} is not a built-in theme and has no matching [[ui.custom_theme]] id",
            cfg.ui.theme
        )));
    }
    if !matches!(cfg.ui.key_hints.as_str(), "rich" | "compact" | "hidden") {
        return Err(CheckError::Validation(format!(
            "[ui].key_hints must be rich|compact|hidden (got {:?})",
            cfg.ui.key_hints
        )));
    }
    // Validate every custom theme's required hex colours so a malformed
    // palette is caught at `config check` time rather than silently
    // skipped by the TUI.
    for c in &cfg.ui.custom_theme {
        if c.id.trim().is_empty() {
            return Err(CheckError::Validation(
                "[[ui.custom_theme]] entry has an empty id".to_string(),
            ));
        }
        let check_hex = |key: &str, val: &str| -> Result<(), CheckError> {
            if !is_hex_color(val) {
                return Err(CheckError::Validation(format!(
                    "[[ui.custom_theme]] {:?}: {key} = {val:?} is not a #rrggbb colour",
                    c.id
                )));
            }
            Ok(())
        };
        check_hex("bg", &c.bg)?;
        check_hex("fg", &c.fg)?;
        check_hex("muted", &c.muted)?;
        check_hex("primary", &c.primary)?;
        check_hex("ok", &c.ok)?;
        check_hex("warn", &c.warn)?;
        check_hex("error", &c.error)?;
        if let Some(oa) = &c.on_accent {
            check_hex("on_accent", oa)?;
        }
    }
    Ok(())
}

/// True for a `#rrggbb` or `rrggbb` hex colour string.
fn is_hex_color(s: &str) -> bool {
    let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
    h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

fn top_level_sections(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|l| {
            let t = l.trim();
            if let Some(rest) = t.strip_prefix('[') {
                rest.strip_suffix(']').map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

fn render(
    overrides: &CliOverrides,
    env: &EnvSnapshot,
    file: Option<&Config>,
    config_path: Option<&Path>,
) -> String {
    let resolved = resolve_daemon_values(overrides, env, file);
    let file_ref = file.cloned().unwrap_or_default();
    let file = &file_ref;
    let mut out = String::new();

    out.push_str("# lad config show — resolved configuration\n");
    if let Some(p) = config_path {
        out.push_str(&format!("# config file: {}\n", p.display()));
    } else {
        out.push_str("# config file: (none — defaults only)\n");
    }
    out.push('\n');

    out.push_str("[daemon]\n");
    out.push_str(&fmt_pathline(
        "socket_path",
        resolved.socket_override.as_deref(),
        resolved.socket_source,
        "default",
    ));
    out.push_str(&fmt_pathline(
        "state_dir",
        resolved.state_dir_override.as_deref(),
        resolved.state_dir_source,
        "default",
    ));
    out.push_str(&fmt_strline(
        "log_level",
        resolved.log_level.as_str(),
        resolved.log_level_source,
    ));
    out.push_str(&fmt_strline(
        "log_format",
        resolved.log_format.as_str(),
        resolved.log_format_source,
    ));
    let (listen_tcp_source, listen_tcp_value) = if file.daemon.listen_tcp.is_empty() {
        (Source::Default, String::new())
    } else {
        (Source::ConfigFile, file.daemon.listen_tcp.clone())
    };
    out.push_str(&fmt_strline(
        "listen_tcp",
        &listen_tcp_value,
        listen_tcp_source,
    ));

    out.push_str("\n[scheduler]\n");
    let sched_default = la_config::SchedulerConfig::default();
    out.push_str(&format!(
        "global_max_concurrent_runs = {}  # {}\n",
        file.scheduler.global_max_concurrent_runs,
        key_source(
            file.scheduler.global_max_concurrent_runs == sched_default.global_max_concurrent_runs
        )
        .label()
    ));
    out.push_str(&format!(
        "cpu_load_throttle = {}  # {}\n",
        file.scheduler.cpu_load_throttle,
        key_source(file.scheduler.cpu_load_throttle == sched_default.cpu_load_throttle).label()
    ));
    out.push_str(&fmt_pathline(
        "archive_dir",
        file.scheduler.archive_dir.as_deref(),
        if file.scheduler.archive_dir.is_some() {
            Source::ConfigFile
        } else {
            Source::Default
        },
        "default",
    ));

    out.push_str("\n[worktree]\n");
    let worktree_default = la_config::WorktreeConfig::default();
    out.push_str(&format!(
        "prune_after_days = {}  # {}\n",
        file.worktree.prune_after_days,
        key_source(file.worktree.prune_after_days == worktree_default.prune_after_days).label()
    ));

    out.push_str("\n[adapters.claude]\n");
    push_adapter(&mut out, &file.adapters.claude.base, &claude_default());
    out.push_str("\n[adapters.codex]\n");
    push_adapter(&mut out, &file.adapters.codex.base, &codex_default().base);
    out.push_str(&format!(
        "prefer_json_mode = {}  # {}\n",
        file.adapters.codex.prefer_json_mode,
        key_source(file.adapters.codex.prefer_json_mode == CodexConfig::default().prefer_json_mode)
            .label()
    ));
    out.push_str("\n[adapters.opencode]\n");
    push_adapter(&mut out, &file.adapters.opencode.base, &opencode_default());
    for (id, cfg) in &file.adapters.extra {
        out.push_str(&format!("\n[adapters.{id}]\n"));
        // Whole table came from the file (since the default has no entry
        // for this id); but still emit per-key labels in case the user
        // left optional keys empty — that way the output shape matches
        // the typed adapters above.
        push_adapter(&mut out, cfg, &AdapterConfig::default());
    }

    out.push_str("\n[ui]\n");
    let ui_default = la_config::UiConfig::default();
    out.push_str(&format!(
        "theme = {:?}  # {}\n",
        file.ui.theme,
        key_source(file.ui.theme == ui_default.theme).label()
    ));
    out.push_str(&format!(
        "key_hints = {:?}  # {}\n",
        file.ui.key_hints,
        key_source(file.ui.key_hints == ui_default.key_hints).label()
    ));
    out.push_str(&format!(
        "compact = {}  # {}\n",
        file.ui.compact,
        key_source(file.ui.compact == ui_default.compact).label()
    ));

    out
}

/// Per-key source label helper: a value that matches the built-in
/// default is reported as `Default`, anything else came from the file
/// (CLI / env layers for these keys are not supported in v1). Used for
/// `[scheduler] / [worktree] / [adapters.*] / [ui]` so a user who set
/// only `cpu_load_throttle = 6.0` does not see `global_max_concurrent_runs`
/// mis-labelled as `from config`.
fn key_source(matches_default: bool) -> Source {
    if matches_default {
        Source::Default
    } else {
        Source::ConfigFile
    }
}

fn fmt_pathline(key: &str, value: Option<&Path>, source: Source, default_label: &str) -> String {
    match value {
        Some(p) => format!(
            "{key} = {:?}  # {}\n",
            p.display().to_string(),
            source.label()
        ),
        None => format!("{key} = \"{default_label}\"  # {}\n", source.label()),
    }
}

fn fmt_strline(key: &str, value: &str, source: Source) -> String {
    format!("{key} = {:?}  # {}\n", value, source.label())
}

fn claude_default() -> AdapterConfig {
    ClaudeConfig::default().base
}
fn codex_default() -> CodexConfig {
    CodexConfig::default()
}
fn opencode_default() -> AdapterConfig {
    OpencodeConfig::default().base
}

fn push_adapter(out: &mut String, current: &AdapterConfig, default: &AdapterConfig) {
    out.push_str(&format!(
        "command = {:?}  # {}\n",
        current.command.clone().unwrap_or_default(),
        key_source(current.command == default.command).label()
    ));
    out.push_str(&format!(
        "extra_args = {:?}  # {}\n",
        current.extra_args,
        key_source(current.extra_args == default.extra_args).label()
    ));
    if !current.env.is_empty() {
        let map: BTreeMap<_, _> = current.env.iter().collect();
        out.push_str(&format!(
            "env = {:?}  # {}\n",
            map,
            // Non-empty env always comes from the config file (default
            // is empty); no need for a key_source call.
            Source::ConfigFile.label()
        ));
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

static LOG_DEPRECATION_EMITTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn emit_log_deprecation_warning() {
    // Fire once per process. Two channels because the env-snapshot is
    // built before tracing is initialised — the stderr line is the
    // user-visible message during bootstrap, the tracing call lights up
    // the structured log once the subscriber is online.
    if LOG_DEPRECATION_EMITTED.set(()).is_err() {
        return;
    }
    let msg = "LAZYAGENTS_LOG is deprecated, use LAZYAGENTS_LOG_LEVEL";
    eprintln!("warning: {msg}");
    tracing::warn!(
        env = ENV_LOG_LEVEL_DEPRECATED,
        replacement = ENV_LOG_LEVEL,
        "{msg}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_unknown_fields_rejects_typo() {
        let raw = r#"
[daemon]
sokcet_path = "/oops"
"#;
        let err = check_str(raw).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown field"), "got: {msg}");
        assert!(msg.contains("sokcet_path"), "got: {msg}");
    }

    #[test]
    fn deny_unknown_fields_rejects_unknown_section() {
        let raw = r#"
[unknown_section]
foo = "bar"
"#;
        let err = check_str(raw).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown field"), "got: {msg}");
        assert!(msg.contains("unknown_section"), "got: {msg}");
    }

    #[test]
    fn deny_unknown_fields_rejects_unknown_key_inside_typed_adapter() {
        // Reviewer-flagged serde gotcha: `#[serde(flatten)]` of an inner
        // struct that itself carries `deny_unknown_fields` can silently
        // swallow stray keys. Make sure `[adapters.codex].wrong_key`
        // actually fails — if it ever stops failing, switch the per-
        // adapter structs to inline fields instead of flatten.
        let raw = r#"
[adapters.codex]
wrong_key = 1
"#;
        let err = check_str(raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown field") && msg.contains("wrong_key"),
            "expected serde to reject wrong_key under [adapters.codex]; got: {msg}"
        );
    }

    #[test]
    fn unknown_adapter_section_is_accepted_and_lands_in_extra() {
        // Forward-compat (reviewer item 1, option A): a config that
        // names a brand-new backend the daemon doesn't yet typecheck
        // must `lad config check` clean today so users can stage the
        // value ahead of a daemon upgrade. Runtime rejection of the
        // unknown adapter is the daemon's job, not the schema's.
        let raw = r#"
[adapters.gemini]
command = "gemini"
extra_args = ["--json"]
"#;
        let cfg = check_str(raw).expect("unknown adapter must not block config check");
        let gemini = cfg
            .adapters
            .extra
            .get("gemini")
            .expect("unknown adapter should land in adapters.extra");
        assert_eq!(gemini.command.as_deref(), Some("gemini"));
        assert_eq!(gemini.extra_args, vec!["--json".to_string()]);
    }

    #[test]
    fn cli_beats_env_beats_file_beats_default() {
        let file = Config {
            daemon: la_config::DaemonConfig {
                log_level: LogLevel::Warn,
                ..Default::default()
            },
            ..Default::default()
        };
        // CLI wins.
        let r = resolve_daemon_values(
            &CliOverrides {
                log_level: Some("trace".into()),
                ..Default::default()
            },
            &EnvSnapshot {
                log_level: Some("debug".into()),
                ..Default::default()
            },
            Some(&file),
        );
        assert_eq!(r.log_level, LogLevel::Trace);
        assert_eq!(r.log_level_source, Source::Cli);

        // Env wins when no CLI.
        let r = resolve_daemon_values(
            &CliOverrides::default(),
            &EnvSnapshot {
                log_level: Some("debug".into()),
                ..Default::default()
            },
            Some(&file),
        );
        assert_eq!(r.log_level, LogLevel::Debug);
        assert_eq!(r.log_level_source, Source::Env);

        // File wins when no CLI / env.
        let r = resolve_daemon_values(
            &CliOverrides::default(),
            &EnvSnapshot::default(),
            Some(&file),
        );
        assert_eq!(r.log_level, LogLevel::Warn);
        assert_eq!(r.log_level_source, Source::ConfigFile);

        // Default wins when nothing set (no file on disk).
        let r = resolve_daemon_values(&CliOverrides::default(), &EnvSnapshot::default(), None);
        assert_eq!(r.log_level, LogLevel::Info);
        assert_eq!(r.log_level_source, Source::Default);
    }

    #[test]
    fn check_str_accepts_example_template() {
        // The committed example must parse — addendum A1/M4.0.5 CI hook
        // pivots on this exact assertion.
        let raw = include_str!("../templates/config.example.toml");
        let cfg = check_str(raw).expect("example template must validate");
        assert_eq!(cfg.daemon.log_level, LogLevel::Info);
        assert_eq!(cfg.daemon.log_format, LogFormat::Json);
        assert_eq!(cfg.scheduler.global_max_concurrent_runs, 8);
    }

    #[test]
    fn rejects_listen_tcp_in_v1() {
        let raw = r#"
[daemon]
listen_tcp = "0.0.0.0:7042"
"#;
        let err = check_str(raw).unwrap_err();
        assert!(format!("{err}").contains("listen_tcp"));
    }

    #[test]
    fn accepts_builtin_named_theme() {
        let raw = "[ui]\ntheme = \"catppuccin-mocha\"\n";
        check_str(raw).expect("built-in named theme must validate");
    }

    #[test]
    fn rejects_unknown_theme_without_custom_definition() {
        let raw = "[ui]\ntheme = \"moonbeam\"\n";
        let err = check_str(raw).unwrap_err();
        assert!(
            format!("{err}").contains("not a built-in theme"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn accepts_theme_backed_by_custom_definition() {
        let raw = r##"[ui]
theme = "my-theme"

[[ui.custom_theme]]
id = "my-theme"
bg = "#1e1e2e"
fg = "#cdd6f4"
muted = "#a6adc8"
primary = "#89b4fa"
ok = "#a6e3a1"
warn = "#f9e2af"
error = "#f38ba8"
"##;
        check_str(raw).expect("theme matching a custom_theme id must validate");
    }

    #[test]
    fn rejects_custom_theme_with_bad_hex() {
        let raw = r##"[ui]
theme = "auto"

[[ui.custom_theme]]
id = "x"
bg = "not-a-color"
fg = "#ffffff"
muted = "#808080"
primary = "#89b4fa"
ok = "#a6e3a1"
warn = "#f9e2af"
error = "#f38ba8"
"##;
        let err = check_str(raw).unwrap_err();
        assert!(
            format!("{err}").contains("not a #rrggbb colour"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn show_labels_each_scheduler_key_independently() {
        // Reviewer item 2: a user who set ONLY `cpu_load_throttle = 6.0`
        // must see `global_max_concurrent_runs = 8  # from default`
        // (not `# from config`) — the per-key labelling fix lives or
        // dies on this case.
        let raw = "[scheduler]\ncpu_load_throttle = 6.0\n";
        let file = Config::parse_str(raw).unwrap();
        let rendered = render(
            &CliOverrides::default(),
            &EnvSnapshot::default(),
            Some(&file),
            None,
        );
        assert!(
            rendered.contains("cpu_load_throttle = 6  # from config"),
            "expected cpu_load_throttle labelled 'from config': {rendered}"
        );
        assert!(
            rendered.contains("global_max_concurrent_runs = 8  # from default"),
            "expected global_max_concurrent_runs labelled 'from default': {rendered}"
        );
    }

    #[test]
    fn show_includes_unknown_adapter_sections_from_extra() {
        let raw = r#"
[adapters.gemini]
command = "gemini"
"#;
        let file = Config::parse_str(raw).unwrap();
        let rendered = render(
            &CliOverrides::default(),
            &EnvSnapshot::default(),
            Some(&file),
            None,
        );
        assert!(
            rendered.contains("[adapters.gemini]"),
            "expected unknown adapter table in show output: {rendered}"
        );
        assert!(
            rendered.contains("command = \"gemini\"  # from config"),
            "expected per-key config source label: {rendered}"
        );
    }

    #[test]
    fn validate_cli_env_enums_rejects_invalid_cli_log_level() {
        let overrides = CliOverrides {
            log_level: Some("typo".into()),
            ..Default::default()
        };
        let err = validate_cli_env_enums(&overrides, &EnvSnapshot::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--log-level") && msg.contains("typo"),
            "got: {msg}"
        );
    }

    #[test]
    fn validate_cli_env_enums_rejects_invalid_env_log_format() {
        let env = EnvSnapshot {
            log_format: Some("yaml".into()),
            ..Default::default()
        };
        let err = validate_cli_env_enums(&CliOverrides::default(), &env).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("LAZYAGENTS_LOG_FORMAT") && msg.contains("yaml"),
            "got: {msg}"
        );
    }

    #[test]
    fn validate_cli_env_enums_accepts_canonical_values() {
        let overrides = CliOverrides {
            log_level: Some("debug".into()),
            log_format: Some("compact".into()),
            ..Default::default()
        };
        let env = EnvSnapshot {
            log_level: Some("WARN".into()),
            log_format: Some("json".into()),
            ..Default::default()
        };
        validate_cli_env_enums(&overrides, &env).unwrap();
    }

    #[test]
    fn check_rejects_explicit_missing_path_but_accepts_implicit_missing() {
        // Reviewer round 3 blocker: `lad config check --config X.toml`
        // where X.toml doesn't exist must fail loudly so the A1/M4.0.5
        // CI gate cannot silently pass after the template gets deleted
        // or the runner cwd shifts. Implicit (no `--config` flag, no
        // env override, resolver default also absent) stays Ok so a
        // fresh checkout without a user config still passes.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist.toml");

        // explicit=true → error
        let err = check(Some(&missing), true).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("does not exist") && msg.contains("does-not-exist.toml"),
            "expected explicit-missing error to name path: {msg}"
        );

        // explicit=false → ok(existed=false)
        let summary = check(Some(&missing), false).unwrap();
        assert!(!summary.existed);
        assert_eq!(summary.path, missing);
    }
}
