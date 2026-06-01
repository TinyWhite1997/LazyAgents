//! `lad` — LazyAgents daemon binary (WEK-21 / M1.7).
//!
//! Subcommands:
//!
//! - `lad start` — foreground run, binds the per-version UDS, serves RPC,
//!   exits cleanly on SIGINT/SIGTERM.
//! - `lad daemonize` — fork-and-setsid into a detached `start`; waits for
//!   the socket to appear before exiting 0.
//! - `lad metrics` — placeholder (post-M1; prints a stub for now so
//!   `lad --help` lists it).
//! - `lad doctor` — prints the resolved socket path + state dir and
//!   reports whether a daemon is already alive.
//!
//! The binary stays small on purpose — all logic lives in `la-daemon` so
//! the integration suite can spin up a daemon in-process without invoking
//! the binary at all.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use la_adapter::{claude::ClaudeAdapter, AgentAdapter};
use la_daemon::paths::{ensure_runtime_dir, SocketDiscovery, SocketLocation};
use la_daemon::{
    spawn_daemonized, Daemon, DaemonConfig, DaemonError, DaemonizeError, SERVER_VERSION,
};

const HELP: &str = "\
lad — LazyAgents daemon (WEK-21 / M1.7)

USAGE:
    lad <command> [flags]

COMMANDS:
    start              Run the daemon in the foreground.
    daemonize          Fork into a detached background process.
    metrics            (stub) Print the active daemon's metrics.
    doctor             Diagnose socket / state paths and reachability.

GLOBAL FLAGS:
    --socket <path>    Override the socket path. Default = $LAZYAGENTS_RUNTIME_DIR
                       or $XDG_RUNTIME_DIR/lazyagents/lad-<protocol>.sock.
    --state-dir <dir>  Override the state (SQLite) directory.
    --log-level <lvl>  trace|debug|info|warn|error (default info).
    -h, --help         Show this help.
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
    log_level: String,
}

enum Command {
    Start,
    Daemonize,
    Metrics,
    Doctor,
    Help,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut iter = args.iter().skip(1);
    let cmd_raw = iter.next().cloned().unwrap_or_else(|| "help".to_string());
    let cmd = match cmd_raw.as_str() {
        "start" => Command::Start,
        "daemonize" => Command::Daemonize,
        "metrics" => Command::Metrics,
        "doctor" => Command::Doctor,
        "-h" | "--help" | "help" => Command::Help,
        other => return Err(format!("unknown command: {other}")),
    };

    let mut socket_override = None;
    let mut state_dir_override = None;
    let mut log_level = std::env::var("LAZYAGENTS_LOG").unwrap_or_else(|_| "info".to_string());

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
                log_level = iter
                    .next()
                    .cloned()
                    .ok_or_else(|| "--log-level expects a value".to_string())?;
            }
            "-h" | "--help" => {
                return Ok(Parsed {
                    cmd: Command::Help,
                    socket_override,
                    state_dir_override,
                    log_level,
                });
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    Ok(Parsed {
        cmd,
        socket_override,
        state_dir_override,
        log_level,
    })
}

fn init_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn run(p: Parsed) -> ExitCode {
    match p.cmd {
        Command::Help => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Command::Doctor => {
            let loc = resolve_socket(&p.socket_override);
            let state_dir = p
                .state_dir_override
                .clone()
                .unwrap_or_else(la_daemon::default_state_dir);
            println!("socket path:    {}", loc.socket_path.display());
            println!("runtime dir:    {}", loc.runtime_dir.display());
            println!("state dir:      {}", state_dir.display());
            println!("server version: {SERVER_VERSION}");
            #[cfg(unix)]
            {
                use std::os::unix::net::UnixStream;
                match UnixStream::connect(&loc.socket_path) {
                    Ok(_) => println!("status:         a daemon is already listening"),
                    Err(_) => println!("status:         no daemon listening"),
                }
            }
            ExitCode::SUCCESS
        }
        Command::Metrics => {
            eprintln!("lad metrics: not yet implemented (tracked under M3 observability work).");
            ExitCode::from(3)
        }
        Command::Daemonize => {
            init_tracing(&p.log_level);
            let loc = resolve_socket(&p.socket_override);
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
            if let Some(path) = &p.socket_override {
                passthrough.push("--socket".to_string());
                passthrough.push(path.display().to_string());
            }
            if let Some(dir) = &p.state_dir_override {
                passthrough.push("--state-dir".to_string());
                passthrough.push(dir.display().to_string());
            }
            passthrough.push("--log-level".to_string());
            passthrough.push(p.log_level.clone());

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
            init_tracing(&p.log_level);
            match run_foreground(p) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("lad: {err}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

fn run_foreground(p: Parsed) -> Result<(), DaemonError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_workers())
        .build()
        .map_err(DaemonError::Io)?;

    runtime.block_on(async move {
        let socket = match &p.socket_override {
            Some(path) => SocketDiscovery::with_override(path.clone()),
            None => SocketDiscovery::default(),
        };
        let state_dir = p
            .state_dir_override
            .clone()
            .unwrap_or_else(la_daemon::default_state_dir);

        let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
        adapters.insert(
            "claude".to_string(),
            Arc::new(ClaudeAdapter::new()) as Arc<dyn AgentAdapter>,
        );

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
