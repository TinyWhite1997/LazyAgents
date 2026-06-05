//! `lad` — LazyAgents daemon binary (WEK-21 / M1.7).
//!
//! Subcommands:
//!
//! - `lad start` — foreground run, binds the per-version UDS, serves RPC,
//!   exits cleanly on SIGINT/SIGTERM.
//! - `lad daemonize` — fork-and-setsid into a detached `start`; waits for
//!   the socket to appear before exiting 0.
//! - `lad metrics` — scrape the active daemon's Prometheus text metrics
//!   over a local Unix-domain socket.
//! - `lad doctor` — prints the resolved socket path + state dir and
//!   reports whether a daemon is already alive.
//! - `lad backup --output <path>` — writes a consistent SQLite snapshot.
//!
//! The binary stays small on purpose — all logic lives in `la-daemon` so
//! the integration suite can spin up a daemon in-process without invoking
//! the binary at all.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use la_adapter::{
    claude::ClaudeAdapter, codex::CodexAdapter, opencode::OpencodeAdapter, AgentAdapter,
    ProbeResult,
};
#[cfg(debug_assertions)]
use la_adapter::{AdapterDescriptor, SpawnRequest, SpawnSpec};
use la_daemon::paths::{ensure_runtime_dir, SocketDiscovery, SocketLocation};
use la_daemon::{
    spawn_daemonized, Daemon, DaemonConfig, DaemonError, DaemonizeError, SERVER_VERSION,
};

const HELP: &str = "\
lad — LazyAgents daemon (WEK-21 / M1.7 + WEK-71 / M4.2 + WEK-73 / M4.1)

USAGE:
    lad <command> [flags]

COMMANDS:
    start              Run the daemon in the foreground.
    daemonize          Fork into a detached background process.
    metrics            Print the active daemon's Prometheus text metrics.
    doctor             Diagnose socket / state paths and reachability.
    backup             Write a consistent SQLite snapshot to --output <path>.
    config show        Print the merged configuration with provenance.
    config check       Parse + validate config.toml without starting daemon.
    config path        Print the config path resolve_config_path() would read.
    install            Install a service unit (systemd / launchd / Windows task).
    uninstall          Remove the service unit (idempotent).

GLOBAL FLAGS:
    --socket <path>    Override the socket path. Default = $LAZYAGENTS_SOCKET
                       or $LAZYAGENTS_RUNTIME_DIR/lazyagents/lad-<protocol>.sock.
    --state-dir <dir>  Override the state (SQLite) directory.
    --log-level <lvl>  trace|debug|info|warn|error (default info).
    --log-format <fmt> json|compact (default json).
    --config <path>    Override the config file (default = resolve_config_path()).
    -h, --help         Show this help.

INSTALL FLAGS (install / uninstall):
    --service <mode>   systemd | launchd | windows-task   (required)
    --enable           install: also `enable` the unit (start-at-login).
    --start            install: also `start` the unit now.
    --dry-run          install/uninstall: log every action; touch nothing.

ENVIRONMENT:
    LAZYAGENTS_SOCKET / LAZYAGENTS_STATE_DIR / LAZYAGENTS_LOG_LEVEL /
    LAZYAGENTS_LOG_FORMAT / LAZYAGENTS_CONFIG override the matching flags.
    LAZYAGENTS_LOG is a deprecated alias for LAZYAGENTS_LOG_LEVEL
    (warning emitted; removed in v1.1).
    LAZYAGENTS_MANAGED_BY is set by service units (systemd / launchd /
    windows-task) and tells the la bootstrap to skip auto-daemonize.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match parse(&args) {
        Ok(parsed) => run(parsed),
        Err(err) => {
            eprintln!("lad: {err}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

struct Parsed {
    cmd: Command,
    socket_override: Option<std::path::PathBuf>,
    state_dir_override: Option<std::path::PathBuf>,
    /// CLI-supplied log level (None = no `--log-level`); env/file
    /// fallbacks are applied in [`derive_runtime`].
    cli_log_level: Option<String>,
    /// CLI-supplied log format (None = no `--log-format`).
    cli_log_format: Option<String>,
    /// CLI-supplied config path (`--config <path>`).
    cli_config_path: Option<std::path::PathBuf>,
    #[cfg(debug_assertions)]
    test_shell_adapter_script: Option<String>,
}

enum Command {
    Start,
    Daemonize,
    Metrics,
    Doctor,
    Backup { output: std::path::PathBuf },
    Config { sub: ConfigSub },
    Install(InstallSpec),
    Uninstall(UninstallSpec),
    Help,
}

#[derive(Default)]
struct InstallSpec {
    /// `--service <mode>`; required when the user runs `lad install`.
    service: Option<String>,
    enable: bool,
    start: bool,
    dry_run: bool,
}

#[derive(Default)]
struct UninstallSpec {
    service: Option<String>,
    dry_run: bool,
}

enum ConfigSub {
    Show,
    Check,
    Path,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut iter = args.iter().skip(1);
    let cmd_raw = iter.next().cloned().unwrap_or_else(|| "help".to_string());
    let mut cmd = match cmd_raw.as_str() {
        "start" => Command::Start,
        "daemonize" => Command::Daemonize,
        "metrics" => Command::Metrics,
        "doctor" => Command::Doctor,
        "backup" => Command::Backup {
            output: std::path::PathBuf::new(),
        },
        "config" => {
            let sub_raw = iter
                .next()
                .cloned()
                .ok_or_else(|| "config requires a subcommand: show | check | path".to_string())?;
            let sub = match sub_raw.as_str() {
                "show" => ConfigSub::Show,
                "check" => ConfigSub::Check,
                "path" => ConfigSub::Path,
                other => return Err(format!("unknown config subcommand: {other}")),
            };
            Command::Config { sub }
        }
        "install" => Command::Install(InstallSpec::default()),
        "uninstall" => Command::Uninstall(UninstallSpec::default()),
        "-h" | "--help" | "help" => Command::Help,
        other => return Err(format!("unknown command: {other}")),
    };

    let mut socket_override = None;
    let mut state_dir_override = None;
    let mut cli_log_level: Option<String> = None;
    let mut cli_log_format: Option<String> = None;
    let mut cli_config_path: Option<std::path::PathBuf> = None;
    #[cfg(debug_assertions)]
    let mut test_shell_adapter_script = None;

    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--socket" => {
                socket_override = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--socket expects a path".to_string())?
                        .into(),
                );
            }
            "--state-dir" => {
                state_dir_override = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--state-dir expects a path".to_string())?
                        .into(),
                );
            }
            "--log-level" => {
                cli_log_level = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--log-level expects a value".to_string())?,
                );
            }
            "--log-format" => {
                cli_log_format = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--log-format expects a value".to_string())?,
                );
            }
            "--config" => {
                cli_config_path = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--config expects a path".to_string())?
                        .into(),
                );
            }
            "--output" => match &mut cmd {
                Command::Backup { output } => {
                    *output = iter
                        .next()
                        .cloned()
                        .ok_or_else(|| "--output expects a path".to_string())?
                        .into();
                }
                _ => return Err("--output is only valid for backup".to_string()),
            },
            "--service" => {
                let value = iter
                    .next()
                    .cloned()
                    .ok_or_else(|| "--service expects a mode".to_string())?;
                match &mut cmd {
                    Command::Install(spec) => spec.service = Some(value),
                    Command::Uninstall(spec) => spec.service = Some(value),
                    _ => return Err("--service is only valid for install/uninstall".to_string()),
                }
            }
            "--enable" => match &mut cmd {
                Command::Install(spec) => spec.enable = true,
                _ => return Err("--enable is only valid for install".to_string()),
            },
            "--start" => match &mut cmd {
                Command::Install(spec) => spec.start = true,
                _ => return Err("--start is only valid for install".to_string()),
            },
            "--dry-run" => match &mut cmd {
                Command::Install(spec) => spec.dry_run = true,
                Command::Uninstall(spec) => spec.dry_run = true,
                _ => return Err("--dry-run is only valid for install/uninstall".to_string()),
            },
            #[cfg(debug_assertions)]
            "--test-shell-adapter" => {
                test_shell_adapter_script =
                    Some(iter.next().cloned().ok_or_else(|| {
                        "--test-shell-adapter expects a shell script".to_string()
                    })?);
            }
            "-h" | "--help" => {
                return Ok(Parsed {
                    cmd: Command::Help,
                    socket_override,
                    state_dir_override,
                    cli_log_level,
                    cli_log_format,
                    cli_config_path,
                    #[cfg(debug_assertions)]
                    test_shell_adapter_script,
                });
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    Ok(Parsed {
        cmd,
        socket_override,
        state_dir_override,
        cli_log_level,
        cli_log_format,
        cli_config_path,
        #[cfg(debug_assertions)]
        test_shell_adapter_script,
    })
}

fn init_observability(level: &str, format: la_config::LogFormat, state_dir: &std::path::Path) {
    // M4.5 / WEK-75 — A9 tracing JSON format 统一: JSON is the production
    // default (`tracing-subscriber::fmt().json()` with the pinned field
    // set in la-observ::init_json_tracing); `--log-format compact` /
    // `LAZYAGENTS_LOG_FORMAT=compact` swaps in the human-readable layer
    // for developer tty use.
    match format {
        la_config::LogFormat::Json => la_observ::init_json_tracing(level),
        la_config::LogFormat::Compact => la_observ::init_compact_tracing(level),
    }
    la_observ::install_metrics_recorder();
    la_observ::install_crash_reporter(state_dir.join("crashes"));
}

/// Build a [`la_daemon::config_cmd::CliOverrides`] mirror of the
/// parsed CLI flags. Centralised so every command (`start`,
/// `daemonize`, `config show`, ...) sees the same precedence rules.
fn cli_overrides(p: &Parsed) -> la_daemon::config_cmd::CliOverrides {
    la_daemon::config_cmd::CliOverrides {
        socket: p.socket_override.clone(),
        state_dir: p.state_dir_override.clone(),
        log_level: p.cli_log_level.clone(),
        log_format: p.cli_log_format.clone(),
        config_path: p.cli_config_path.clone(),
    }
}

/// Load the file (if it exists), snapshot env, and derive the
/// CLI > env > file > default values used by `start` / `daemonize`.
fn derive_runtime(p: &Parsed) -> Result<la_daemon::config_cmd::ResolvedDaemonValues, String> {
    let overrides = cli_overrides(p);
    let env = la_daemon::config_cmd::EnvSnapshot::from_process();
    // Reject typo'd `--log-level` / `LAZYAGENTS_LOG_FORMAT` etc. *before*
    // we silently demote them to None and fall back to the file/default
    // layer — otherwise an unrecognised value disappears without an
    // error and `lad config show` would mis-label the resolved value
    // as `from default`.
    la_daemon::config_cmd::validate_cli_env_enums(&overrides, &env).map_err(|e| e.to_string())?;
    let config_path = overrides
        .config_path
        .clone()
        .or_else(|| env.config.clone())
        .or_else(|| la_config::resolve_config_path().existing);
    let file = match config_path.as_deref() {
        Some(path) if path.exists() => Some(
            la_config::Config::load_file(path)
                .map_err(|e| format!("config parse failed at {}: {e}", path.display()))?,
        ),
        _ => None,
    };
    // Share `lad config check`'s cross-field validation with the daemon
    // startup paths so `[daemon].listen_tcp = "..."` cannot pass `check`
    // but be silently ignored by `start` (reviewer-flagged
    // "configured but nothing happened" hole).
    if let Some(cfg) = file.as_ref() {
        la_daemon::config_cmd::validate(cfg).map_err(|e| {
            let p = config_path
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<no path>".to_string());
            format!("config validation failed at {p}: {e}")
        })?;
    }
    Ok(la_daemon::config_cmd::resolve_daemon_values(
        &overrides,
        &env,
        file.as_ref(),
    ))
}

fn run(p: Parsed) -> ExitCode {
    match &p.cmd {
        Command::Help => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Command::Config { sub } => run_config(sub, &p),
        Command::Install(spec) => run_install_cmd(spec, &p),
        Command::Uninstall(spec) => run_uninstall_cmd(spec),
        Command::Doctor => {
            let resolved = match derive_runtime(&p) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("lad doctor: {err}");
                    return ExitCode::from(1);
                }
            };
            let loc = resolve_socket(&resolved.socket_override);
            let state_dir = resolved
                .state_dir_override
                .clone()
                .unwrap_or_else(la_daemon::default_state_dir);
            let adapter_pairs: Vec<(&'static str, std::sync::Arc<dyn AgentAdapter>)> = vec![
                ("claude", std::sync::Arc::new(ClaudeAdapter::new())),
                ("codex", std::sync::Arc::new(CodexAdapter::new())),
                ("opencode", std::sync::Arc::new(OpencodeAdapter::new())),
            ];
            let doctor_adapters: Vec<la_daemon::doctor::DoctorAdapter> = adapter_pairs
                .into_iter()
                .map(|(id, a)| la_daemon::doctor::DoctorAdapter {
                    id: id.to_string(),
                    adapter: a,
                    // The CLI runs out-of-process from the daemon; we
                    // don't have the daemon's HealthRegistry on hand,
                    // so we leave the "last probe N ago" cell blank.
                    // The probe below is fresh by definition, so the
                    // user still sees current state.
                    last_probe_age: None,
                })
                .collect();
            let adapter_probes = la_daemon::doctor::probe_adapters_sync(&doctor_adapters);
            let inputs = la_daemon::doctor::DoctorInputs {
                socket_path: loc.socket_path.clone(),
                state_dir: state_dir.clone(),
                server_version: SERVER_VERSION.to_string(),
                running_daemon_version: query_running_daemon_version(&loc.socket_path),
                state_dir_free_bytes: la_daemon::doctor::detect_state_dir_free_bytes(&state_dir),
                adapters: doctor_adapters,
                git_version: la_daemon::doctor::detect_git_version(),
                daemon_reachable: la_daemon::doctor::probe_daemon_reachable(&loc.socket_path),
                adapter_probes,
            };
            let report = la_daemon::doctor::run_with_inputs(&inputs);
            print!("{}", report.render());
            ExitCode::from(report.tier.code())
        }
        Command::Backup { output } => {
            let output = output.clone();
            if output.as_os_str().is_empty() {
                eprintln!("lad: backup requires --output <path>");
                return ExitCode::from(2);
            }
            let resolved = match derive_runtime(&p) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("lad backup: {err}");
                    return ExitCode::from(1);
                }
            };
            let state_dir = resolved
                .state_dir_override
                .clone()
                .unwrap_or_else(la_daemon::default_state_dir);
            init_observability(resolved.log_level.as_str(), resolved.log_format, &state_dir);
            match run_backup(resolved.state_dir_override.clone(), output) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("lad: backup failed: {err}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Metrics => {
            let resolved = match derive_runtime(&p) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("lad metrics: {err}");
                    return ExitCode::from(1);
                }
            };
            let loc = resolve_socket(&resolved.socket_override);
            match scrape_metrics(&loc.socket_path) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("lad metrics: {err}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Daemonize => {
            let resolved = match derive_runtime(&p) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("lad daemonize: {err}");
                    return ExitCode::from(1);
                }
            };
            let state_dir = resolved
                .state_dir_override
                .clone()
                .unwrap_or_else(la_daemon::default_state_dir);
            init_observability(resolved.log_level.as_str(), resolved.log_format, &state_dir);
            let loc = resolve_socket(&resolved.socket_override);
            if let Err(err) = ensure_runtime_dir(&loc.runtime_dir) {
                eprintln!(
                    "lad: ensure_runtime_dir({}): {err}",
                    loc.runtime_dir.display()
                );
                return ExitCode::from(1);
            }
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(err) => {
                    eprintln!("lad: cannot resolve own exe: {err}");
                    return ExitCode::from(1);
                }
            };
            let mut passthrough = Vec::new();
            if let Some(path) = &resolved.socket_override {
                passthrough.push("--socket".to_string());
                passthrough.push(path.display().to_string());
            }
            if let Some(dir) = &resolved.state_dir_override {
                passthrough.push("--state-dir".to_string());
                passthrough.push(dir.display().to_string());
            }
            passthrough.push("--log-level".to_string());
            passthrough.push(resolved.log_level.as_str().to_string());
            passthrough.push("--log-format".to_string());
            passthrough.push(resolved.log_format.as_str().to_string());
            if let Some(cfg) = &p.cli_config_path {
                passthrough.push("--config".to_string());
                passthrough.push(cfg.display().to_string());
            }
            #[cfg(debug_assertions)]
            if let Some(script) = &p.test_shell_adapter_script {
                passthrough.push("--test-shell-adapter".to_string());
                passthrough.push(script.clone());
            }

            match spawn_daemonized(
                &exe,
                &loc.socket_path,
                &passthrough,
                Duration::from_secs(10),
            ) {
                Ok(outcome) => {
                    println!(
                        "lad daemonized: pid={} socket={}",
                        outcome.pid,
                        outcome.socket_path.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(DaemonizeError::EarlyExit(status)) => {
                    eprintln!("lad: child exited early: {status}");
                    ExitCode::from(1)
                }
                Err(err) => {
                    eprintln!("lad: daemonize failed: {err}");
                    ExitCode::from(1)
                }
            }
        }
        Command::Start => {
            let resolved = match derive_runtime(&p) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("lad start: {err}");
                    return ExitCode::from(1);
                }
            };
            // S1 / 跨平台启停一致性: if a service unit is already installed
            // on this host, and we are NOT being launched by it (no
            // LAZYAGENTS_MANAGED_BY in our env), refuse so we don't end
            // up with two daemons racing on the same socket. The
            // service path is the authoritative one once installed.
            if std::env::var_os("LAZYAGENTS_MANAGED_BY").is_none() {
                if let Some(hint) = detect_local_service_unit() {
                    eprintln!(
                        "lad start: a {} service unit is already installed on this host. \
                         A service manager owns lifecycle once installed; please use \
                         `{}` instead of `lad start`.",
                        hint.label, hint.suggested_cmd
                    );
                    return ExitCode::from(2);
                }
            }
            let state_dir = resolved
                .state_dir_override
                .clone()
                .unwrap_or_else(la_daemon::default_state_dir);
            init_observability(resolved.log_level.as_str(), resolved.log_format, &state_dir);
            match run_foreground(p, resolved) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("lad: {err}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

fn run_config(sub: &ConfigSub, p: &Parsed) -> ExitCode {
    match sub {
        ConfigSub::Show => {
            let overrides = cli_overrides(p);
            match la_daemon::config_cmd::show(&overrides) {
                Ok(rendered) => {
                    print!("{rendered}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("lad config show: {err}");
                    ExitCode::from(1)
                }
            }
        }
        ConfigSub::Check => {
            // "explicit" = user (or CI) asked for THIS file. Either CLI
            // flag `--config <path>` or env `LAZYAGENTS_CONFIG=<path>`
            // counts; only the resolver-default lookup is implicit.
            let env_config = la_daemon::config_cmd::EnvSnapshot::from_process().config;
            let explicit = p.cli_config_path.is_some() || env_config.is_some();
            let path = p
                .cli_config_path
                .clone()
                .or(env_config)
                .or_else(|| la_config::resolve_config_path().existing);
            match la_daemon::config_cmd::check(path.as_deref(), explicit) {
                Ok(summary) => {
                    if summary.existed {
                        println!(
                            "ok: {} sections in {}",
                            summary.sections_present.len(),
                            summary.path.display()
                        );
                    } else {
                        println!(
                            "ok: no config file present at {} (defaults will apply)",
                            summary.path.display()
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("lad config check failed: {err}");
                    if let la_daemon::config_cmd::CheckError::Schema(_) = err {
                        eprintln!(
                            "hint: accepted top-level sections are [daemon], [scheduler], \
                             [worktree], [adapters.claude], [adapters.codex], \
                             [adapters.opencode], [ui]"
                        );
                    }
                    ExitCode::from(1)
                }
            }
        }
        ConfigSub::Path => {
            println!("{}", la_daemon::config_cmd::path_string());
            ExitCode::SUCCESS
        }
    }
}

fn run_install_cmd(spec: &InstallSpec, p: &Parsed) -> ExitCode {
    let Some(raw) = spec.service.as_deref() else {
        eprintln!("lad install: --service <systemd|launchd|windows-task> is required");
        return ExitCode::from(2);
    };
    let mode = match la_daemon::install::ServiceMode::parse(raw) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("lad install: {err}");
            return ExitCode::from(2);
        }
    };
    let args = la_daemon::install::InstallArgs {
        mode,
        enable: spec.enable,
        start: spec.start,
        config_path: p.cli_config_path.clone(),
        dry_run: spec.dry_run,
    };
    match la_daemon::install::run_install(&args) {
        Ok(outcomes) => {
            for o in &outcomes {
                println!("{o}");
            }
            println!();
            println!(
                "{}",
                la_daemon::install::cli::render_post_install_hint(&args)
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lad install: {err}");
            ExitCode::from(1)
        }
    }
}

fn run_uninstall_cmd(spec: &UninstallSpec) -> ExitCode {
    let Some(raw) = spec.service.as_deref() else {
        eprintln!("lad uninstall: --service <systemd|launchd|windows-task> is required");
        return ExitCode::from(2);
    };
    let mode = match la_daemon::install::ServiceMode::parse(raw) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("lad uninstall: {err}");
            return ExitCode::from(2);
        }
    };
    let args = la_daemon::install::UninstallArgs {
        mode,
        dry_run: spec.dry_run,
    };
    match la_daemon::install::run_uninstall(&args) {
        Ok(outcomes) => {
            for o in &outcomes {
                println!("{o}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lad uninstall: {err}");
            ExitCode::from(1)
        }
    }
}

/// M4.5 / WEK-75 — A9 metrics.scrape 三层一致性 (`lad metrics` CLI 层).
///
/// Dial the daemon's main socket (Unix UDS / Windows Named Pipe), run the
/// standard la-ipc handshake, issue a single `metrics.scrape` RPC, and
/// return `result.body` verbatim. The integration test in
/// `crates/la-daemon/tests/acceptance.rs` asserts the bytes returned here
/// are byte-identical to the daemon's `la_observ::render_prometheus()`
/// output so a CLI-side `print!` cannot drop newlines or add prefixes.
fn scrape_metrics(socket_path: &std::path::Path) -> std::io::Result<String> {
    use la_ipc::connection::Connection;
    use la_ipc::handshake::client_handshake;
    use la_ipc::transport::{connect, Endpoint};
    use la_proto::jsonrpc::{Message, Request, RequestId};
    use la_proto::methods::{Method, MetricsScrape, MetricsScrapeParams, MetricsScrapeResult};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    rt.block_on(async move {
        let endpoint = match () {
            #[cfg(unix)]
            () => Endpoint::uds(socket_path),
            // Windows Named Pipe (B2 决议): the daemon binds
            // `\\.\pipe\lazyagents-<stem>` (see `endpoint_for` in
            // runtime.rs); mirror that path here so the CLI talks to
            // the same endpoint regardless of platform.
            #[cfg(not(unix))]
            () => Endpoint::named_pipe(format!(
                r"\\.\pipe\lazyagents-{}",
                socket_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("lad")
            )),
        };
        let stream = tokio::time::timeout(Duration::from_secs(5), connect(&endpoint))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))?
            .map_err(|e| std::io::Error::other(format!("connect: {e}")))?;
        let mut conn = Connection::new(stream);
        tokio::time::timeout(
            Duration::from_secs(5),
            client_handshake(
                &mut conn,
                "lad-metrics",
                SERVER_VERSION,
                &[la_proto::PROTOCOL_VERSION],
            ),
        )
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "handshake timeout"))?
        .map_err(|e| std::io::Error::other(format!("handshake: {e}")))?;

        let req = Request::new(1i64, MetricsScrape::NAME, MetricsScrapeParams::default())
            .map_err(|e| std::io::Error::other(format!("encode request: {e}")))?;
        conn.send(&Message::Request(req))
            .await
            .map_err(|e| std::io::Error::other(format!("send request: {e}")))?;
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), conn.recv())
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "response timeout")
                })?
                .map_err(|e| std::io::Error::other(format!("recv: {e}")))?
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "daemon closed")
                })?;
            if let Message::Response(resp) = msg {
                if resp.id != RequestId::Num(1) {
                    continue;
                }
                return match resp.outcome {
                    la_proto::jsonrpc::ResponseOutcome::Result(v) => {
                        let body: MetricsScrapeResult = serde_json::from_value(v)
                            .map_err(|e| std::io::Error::other(format!("decode result: {e}")))?;
                        Ok(body.body)
                    }
                    la_proto::jsonrpc::ResponseOutcome::Error(err) => Err(std::io::Error::other(
                        format!("metrics.scrape rpc error: code={} {}", err.code, err.message),
                    )),
                };
            }
        }
    })
}

fn run_backup(
    state_dir_override: Option<std::path::PathBuf>,
    output: std::path::PathBuf,
) -> Result<(), DaemonError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(DaemonError::Io)?;
    runtime.block_on(async move {
        let state_dir = state_dir_override.unwrap_or_else(la_daemon::default_state_dir);
        la_storage::Storage::backup_path_to(state_dir.join("lad.sqlite"), &output).await?;
        println!("backup written: {}", output.display());
        Ok(())
    })
}

/// Best-effort fetch of the running daemon's reported version, used by
/// the doctor "daemon version" check. Returns `None` when the daemon
/// isn't reachable or the initialize handshake doesn't complete inside
/// the timeout — both cases collapse to "skip the match" in
/// `doctor::check_daemon_version_match`.
fn query_running_daemon_version(socket: &std::path::Path) -> Option<String> {
    use la_ipc::connection::Connection;
    use la_ipc::handshake::client_handshake;
    use la_ipc::transport::{connect, Endpoint};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .ok()?;
    rt.block_on(async {
        let endpoint = match () {
            #[cfg(unix)]
            () => Endpoint::uds(socket),
            #[cfg(not(unix))]
            () => Endpoint::named_pipe(format!(
                r"\\.\pipe\lazyagents-{}",
                socket.file_stem().and_then(|s| s.to_str()).unwrap_or("lad")
            )),
        };
        let stream = tokio::time::timeout(Duration::from_millis(500), connect(&endpoint))
            .await
            .ok()?
            .ok()?;
        // The transport returns a concrete `tokio::net::UnixStream` /
        // `NamedPipeClient`; wrap it into the framed `Connection` so
        // `client_handshake` can drive the JSON-RPC exchange.
        let mut conn = Connection::new(stream);
        let info = tokio::time::timeout(
            Duration::from_millis(500),
            client_handshake(
                &mut conn,
                "lad-doctor",
                SERVER_VERSION,
                &[la_proto::PROTOCOL_VERSION],
            ),
        )
        .await
        .ok()?
        .ok()?;
        Some(info.server_version)
    })
}

fn run_foreground(
    p: Parsed,
    resolved: la_daemon::config_cmd::ResolvedDaemonValues,
) -> Result<(), DaemonError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_workers())
        .build()
        .map_err(DaemonError::Io)?;

    runtime.block_on(async move {
        let socket = match &resolved.socket_override {
            Some(path) => SocketDiscovery::with_override(path.clone()),
            None => SocketDiscovery::default(),
        };
        let state_dir = resolved
            .state_dir_override
            .clone()
            .unwrap_or_else(la_daemon::default_state_dir);

        let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
        adapters.insert(
            "claude".to_string(),
            Arc::new(ClaudeAdapter::new()) as Arc<dyn AgentAdapter>,
        );
        // M2.6 (WEK-29): also register `codex` and `opencode` so the
        // health probe loop reports their availability and so the TUI
        // can `sessions.create` against them once the user installs
        // them. Adapters are stateless — registering a backend whose
        // CLI is missing is harmless; the probe simply grey-states it.
        adapters.insert(
            "codex".to_string(),
            Arc::new(CodexAdapter::new()) as Arc<dyn AgentAdapter>,
        );
        adapters.insert(
            "opencode".to_string(),
            Arc::new(OpencodeAdapter::new()) as Arc<dyn AgentAdapter>,
        );
        #[cfg(debug_assertions)]
        if let Some(script) = &p.test_shell_adapter_script {
            adapters.insert(
                "shtest".to_string(),
                Arc::new(DebugShellAdapter {
                    script: script.clone(),
                }) as Arc<dyn AgentAdapter>,
            );
        }

        let config = DaemonConfig {
            state_dir,
            socket_discovery: socket,
            adapters,
            server_version: SERVER_VERSION.to_string(),
            ..DaemonConfig::default()
        };
        let daemon = Daemon::bind(config).await?;
        let handle = daemon.handle();

        // Wire SIGINT/SIGTERM to the handle so the accept loop returns.
        let signal_handle = handle.clone();
        tokio::spawn(async move {
            la_daemon::signals::shutdown_token().await;
            tracing::info!("signal received — initiating graceful shutdown");
            signal_handle.shutdown();
        });

        daemon.accept_loop().await;
        Ok::<(), DaemonError>(())
    })
}

fn resolve_socket(socket_override: &Option<std::path::PathBuf>) -> SocketLocation {
    match socket_override {
        Some(path) => SocketDiscovery::with_override(path.clone()).resolve(),
        None => SocketDiscovery::default().resolve(),
    }
}

fn num_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(2)
}

struct LocalService {
    label: &'static str,
    suggested_cmd: &'static str,
}

/// Best-effort check for "is a service unit for `lad` already installed
/// on this host?". Looks for the unit file on disk only — running
/// `systemctl/launchctl/schtasks` from here would be slow and could
/// prompt for permissions.
fn detect_local_service_unit() -> Option<LocalService> {
    use std::env;
    if let Some(home_os) = env::var_os("HOME") {
        let home = std::path::PathBuf::from(&home_os);
        if cfg!(target_os = "linux") {
            let unit = home.join(".config/systemd/user/lad.service");
            if unit.is_file() {
                return Some(LocalService {
                    label: "systemd",
                    suggested_cmd: "systemctl --user start lad.service",
                });
            }
        }
        if cfg!(target_os = "macos") {
            let plist = home.join("Library/LaunchAgents/dev.lazyagents.lad.plist");
            if plist.is_file() {
                return Some(LocalService {
                    label: "launchd",
                    suggested_cmd: "launchctl kickstart -k gui/$UID/dev.lazyagents.lad",
                });
            }
        }
    }
    if cfg!(windows) {
        if let Some(appdata) = env::var_os("APPDATA") {
            let xml = std::path::PathBuf::from(&appdata).join("lazyagents/lad-task.xml");
            if xml.is_file() {
                return Some(LocalService {
                    label: "windows-task",
                    suggested_cmd: "schtasks /Run /TN \\LazyAgents\\lad",
                });
            }
        }
    }
    None
}

#[cfg(debug_assertions)]
struct DebugShellAdapter {
    script: String,
}

#[cfg(debug_assertions)]
#[async_trait::async_trait]
impl AgentAdapter for DebugShellAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "shtest",
            display_name: "Shell Test Backend",
            default_program: "sh",
            docs_url: "https://example.test/shtest",
        }
    }

    async fn probe(&self) -> ProbeResult {
        ProbeResult::Available {
            version: "0.0.0".into(),
        }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, la_adapter::AdapterError> {
        let script_dir = std::env::temp_dir().join("lazyagents-lad-test-scripts");
        std::fs::create_dir_all(&script_dir).map_err(la_adapter::AdapterError::SpawnFailed)?;
        let script_path = script_dir.join(format!("{}.sh", la_storage::new_id()));
        std::fs::write(&script_path, format!("#!/bin/sh\n{}\n", self.script))
            .map_err(la_adapter::AdapterError::SpawnFailed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perm = std::fs::metadata(&script_path)
                .map_err(la_adapter::AdapterError::SpawnFailed)?
                .permissions();
            perm.set_mode(0o700);
            std::fs::set_permissions(&script_path, perm)
                .map_err(la_adapter::AdapterError::SpawnFailed)?;
        }
        Ok(SpawnSpec {
            program: script_path,
            args: vec![],
            env: req.env.clone(),
            cwd: req.cwd.clone(),
            pty: req.pty,
            stdin_mode: req.stdin_mode,
        })
    }

    fn encode_user_input(&self, text: &str) -> bytes::Bytes {
        bytes::Bytes::copy_from_slice(text.replace('\n', "\r").as_bytes())
    }
}
