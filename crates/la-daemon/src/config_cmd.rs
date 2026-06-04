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
    let env = EnvSnapshot::from_process();

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
/// Returns `Ok(rendered_summary)` on success; on failure the caller
/// should print the error and exit non-zero. `deny_unknown_fields` is
/// enforced by the schema structs themselves — a typo surfaces as a
/// [`CheckError::Schema`] carrying the offending field name in toml's
/// human-readable message ("unknown field `sokcet_path`, expected one
/// of …").
pub fn check(path: Option<&Path>) -> Result<CheckSummary, CheckError> {
    let resolved = resolve_config_path();
    let target = path
        .map(Path::to_path_buf)
        .or_else(|| resolved.existing.clone())
        .unwrap_or_else(|| resolved.write_target.clone());

    if !target.exists() {
        // Missing-file is *not* a parse error; report it as a no-op
        // success so CI on a fresh checkout (where the user has not
        // shipped a config) does not block the pipeline.
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

fn validate(cfg: &Config) -> Result<(), CheckError> {
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
    if !matches!(cfg.ui.theme.as_str(), "auto" | "dark" | "light") {
        return Err(CheckError::Validation(format!(
            "[ui].theme must be auto|dark|light (got {:?})",
            cfg.ui.theme
        )));
    }
    if !matches!(cfg.ui.key_hints.as_str(), "rich" | "compact" | "hidden") {
        return Err(CheckError::Validation(format!(
            "[ui].key_hints must be rich|compact|hidden (got {:?})",
            cfg.ui.key_hints
        )));
    }
    Ok(())
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
}
