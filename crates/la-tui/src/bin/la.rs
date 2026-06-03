//! `la` — LazyAgents TUI client.
//!
//! M1.5/M1.6 ship the binary against a [`MockSessionSource`] so the UI
//! could be reviewed before the daemon (M1.7) landed. The TUI itself is
//! still mock-backed for now — wiring the sidebar against live RPC is
//! tracked under M1.8 — but the **auto-daemonize** acceptance from
//! WEK-21 lives here: on startup we probe the daemon socket; if nothing
//! responds we spawn `lad daemonize` and re-probe. The status bar tells
//! the user whether the daemon was already up, was just spawned, or
//! couldn't be reached.
//!
//! Disable the auto-spawn with `LAZYAGENTS_NO_AUTODAEMON=1` — useful for
//! tests that want the "no daemon" fallback explicitly.
//!
//! ## CLI surface
//!
//! Intentionally minimal — `la` is a TUI, not a CLI. Three top-level
//! flags only:
//!   * `--version` / `-V` — print the compiled version and exit
//!   * `--check-update` — pull the latest GitHub Release manifest,
//!     compare against the running binary, print result, exit. Never
//!     auto-installs (WEK-41 acceptance "默认不自动升级"). See
//!     [`la_tui::update_check`] for the policy details.
//!   * `--help` / `-h` — print the flag summary and exit
//!
//! Anything beyond these three drops through to the normal TUI startup
//! path. We hand-roll the parser so the binary doesn't pull `clap` into
//! release builds — keeps the size budget honest for the < 30 MiB
//! acceptance line.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::{Duration, Instant};

use la_ipc::transport::{connect, Endpoint};
use la_ipc::SocketDiscovery;
use la_tui::status::Status;
use la_tui::update_check;
use la_tui::{App, AppMsg, MockSessionSource};
use tokio::runtime::Runtime;

const AUTO_DAEMON_ENV: &str = "LAZYAGENTS_NO_AUTODAEMON";
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Parse result for the top-level CLI flags. Anything other than
/// [`CliAction::RunTui`] short-circuits the TUI startup path.
enum CliAction {
    RunTui,
    PrintVersion,
    CheckUpdate,
    Doctor(Vec<String>),
    PrintHelp,
    /// `--flag-name` was unknown. Surface it as exit 2 so wrapper
    /// scripts (`la --check-updates` typo etc.) fail fast.
    Unknown(String),
}

fn parse_cli() -> CliAction {
    let mut args = std::env::args().skip(1);
    // We only honor the FIRST recognized flag — chaining `--version
    // --check-update` is undefined and not worth the complexity.
    match args.next().as_deref() {
        None => CliAction::RunTui,
        Some("--version" | "-V") => CliAction::PrintVersion,
        Some("--check-update") => CliAction::CheckUpdate,
        Some("doctor") => CliAction::Doctor(args.collect()),
        Some("--help" | "-h") => CliAction::PrintHelp,
        Some(other) if other.starts_with('-') => CliAction::Unknown(other.to_string()),
        // Positional args are reserved for future subcommands (e.g.
        // `la attach <session>`); for now drop straight into the TUI.
        Some(_) => CliAction::RunTui,
    }
}

fn print_help() {
    // The binary is named `la`, the crate is named `la-tui`. Show the
    // user-facing name in the help title to match `--version`'s output
    // ("la X.Y.Z") — no point surfacing the cargo crate name in CLI text.
    let version = env!("CARGO_PKG_VERSION");
    println!("la {version} — LazyAgents TUI client");
    println!();
    println!("USAGE:");
    println!("  la                 launch the TUI (spawns `lad` if not running)");
    println!("  la --version       print version and exit");
    println!("  la --check-update  check GitHub for a newer release and exit");
    println!("  la doctor [flags]  run the daemon health/dependency diagnostics");
    println!("  la --help          print this message");
    println!();
    println!("ENV:");
    println!("  LAZYAGENTS_NO_AUTODAEMON=1   skip auto-spawning `lad daemonize`");
    println!("  LAZYAGENTS_UPDATE_MANIFEST_URL  override the --check-update endpoint");
}

fn main() -> ExitCode {
    match parse_cli() {
        CliAction::RunTui => {
            if let Err(e) = real_main() {
                eprintln!("la: {e}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        CliAction::PrintVersion => {
            println!("la {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        CliAction::PrintHelp => {
            print_help();
            ExitCode::SUCCESS
        }
        CliAction::CheckUpdate => run_check_update(),
        CliAction::Doctor(args) => run_doctor(args),
        CliAction::Unknown(flag) => {
            eprintln!("la: unknown flag `{flag}`. See `la --help`.");
            ExitCode::from(2)
        }
    }
}

fn run_doctor(args: Vec<String>) -> ExitCode {
    match ProcessCommand::new("lad")
        .arg("doctor")
        .args(&args)
        .status()
    {
        Ok(status) => return ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(sibling) = sibling_lad_path() {
                match ProcessCommand::new(&sibling)
                    .arg("doctor")
                    .args(&args)
                    .status()
                {
                    Ok(status) => return ExitCode::from(status.code().unwrap_or(1) as u8),
                    Err(err) => {
                        eprintln!(
                            "la doctor: failed to run `{}` doctor: {err}",
                            sibling.display()
                        );
                    }
                }
            } else {
                eprintln!("la doctor: failed to locate sibling `lad` binary");
            }
        }
        Err(err) => eprintln!("la doctor: failed to run `lad doctor`: {err}"),
    }
    ExitCode::from(1)
}

fn sibling_lad_path() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    path.push(if cfg!(windows) { "lad.exe" } else { "lad" });
    path.exists().then_some(path)
}

fn run_check_update() -> ExitCode {
    let outcome = update_check::check_for_update();
    // Errors go to stderr (non-fatal); successful results go to stdout.
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let render_result = match &outcome {
        update_check::CheckOutcome::Unavailable(_) => update_check::render(&outcome, &mut stderr),
        _ => update_check::render(&outcome, &mut stdout),
    };
    if let Err(e) = render_result {
        eprintln!("la: write check-update result: {e}");
        return ExitCode::from(1);
    }
    ExitCode::from(update_check::exit_code(&outcome))
}

fn real_main() -> io::Result<()> {
    let discovery = SocketDiscovery::default();
    let location = discovery.resolve();

    let bootstrap = bootstrap_daemon(&location.socket_path);

    let source = MockSessionSource::fixture();
    // WEK-42 / M4.3: load `[ui]` from $XDG_CONFIG_HOME/lazyagents/config.toml
    // before instantiating App so the very first frame reflects the
    // user's saved theme + key-hints mode. A missing or unreadable file
    // yields `UiPrefs::default()`; mutations via `T`/`H`/`C` write back
    // to the same path.
    let ui_prefs_path = la_tui::ui_prefs::default_config_path();
    let ui_prefs = ui_prefs_path
        .as_deref()
        .map(la_tui::ui_prefs::load)
        .unwrap_or_default();
    let mut app = App::new(source).with_ui_prefs(ui_prefs, ui_prefs_path);
    // Seed the status bar with what we already know after bootstrap:
    // daemon presence + a right-context note about the socket. Every
    // other field stays at `Status::default()` and is filled in by the
    // first `daemon.health` push the notif-sub thread delivers — WEK-36
    // 验收 "状态栏数据延迟 < 1s" relies on that push, not on a startup
    // guess.
    let mut bootstrap_status = Status::offline();
    bootstrap_status.daemon_online = bootstrap.connected;
    bootstrap_status.right_context = bootstrap.status_context(&location.socket_path);
    app.handle(AppMsg::StatusUpdate(bootstrap_status));

    // WEK-36: subscribe to `daemon.health` AND `cron.fired` over IPC so
    // the status bar + Backends panel reflect real state. The subscriber
    // reconnects with backoff when the connection drops, so a daemon
    // restart auto-recovers without restarting `la`. We start the
    // subscriber even when bootstrap reported no connection: the
    // reconnect loop will keep trying, so once the user runs
    // `lad daemonize` in a sibling shell, the bar lights up on its own.
    let notif_rx = Some(la_tui::notif_sub::spawn(&location.socket_path));
    la_tui::runner::run_with_notifs(app, notif_rx)
}

/// Outcome of the startup bootstrap. Carries enough info for the status
/// bar to tell the user what happened.
struct BootstrapOutcome {
    connected: bool,
    note: BootstrapNote,
}

enum BootstrapNote {
    /// Daemon was already listening — we just connected.
    AlreadyUp,
    /// We spawned `lad daemonize` and the socket appeared.
    Spawned,
    /// Auto-spawn was disabled via env var.
    AutoSpawnDisabled,
    /// `lad` not found on PATH; user must start it manually.
    DaemonBinaryMissing,
    /// We tried to spawn but the daemon didn't come up in time / failed.
    SpawnFailed(String),
}

impl BootstrapOutcome {
    fn status_context(&self, socket: &Path) -> String {
        match &self.note {
            BootstrapNote::AlreadyUp => format!("daemon @ {}", socket.display()),
            BootstrapNote::Spawned => format!("spawned lad @ {}", socket.display()),
            BootstrapNote::AutoSpawnDisabled => {
                format!(
                    "no daemon (LAZYAGENTS_NO_AUTODAEMON set); expected at {}",
                    socket.display()
                )
            }
            BootstrapNote::DaemonBinaryMissing => {
                "no daemon (lad not on PATH); start with `lad daemonize`".to_string()
            }
            BootstrapNote::SpawnFailed(why) => format!("daemon spawn failed: {why}"),
        }
    }
}

fn bootstrap_daemon(socket: &Path) -> BootstrapOutcome {
    // Build the tokio runtime once for the whole bootstrap so the
    // `wait_for_socket` polling loop reuses one reactor instead of
    // spinning a fresh `new_current_thread` runtime every 50 ms (which
    // is wasteful and adds log noise on the spawn path).
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => {
            return BootstrapOutcome {
                connected: false,
                note: BootstrapNote::SpawnFailed("tokio runtime build failed".to_string()),
            }
        }
    };

    if probe_socket(&rt, socket) {
        return BootstrapOutcome {
            connected: true,
            note: BootstrapNote::AlreadyUp,
        };
    }
    if std::env::var_os(AUTO_DAEMON_ENV).is_some() {
        return BootstrapOutcome {
            connected: false,
            note: BootstrapNote::AutoSpawnDisabled,
        };
    }
    let Some(lad) = locate_lad() else {
        return BootstrapOutcome {
            connected: false,
            note: BootstrapNote::DaemonBinaryMissing,
        };
    };
    match spawn_lad_daemonize(&lad) {
        Ok(()) => {}
        Err(err) => {
            return BootstrapOutcome {
                connected: false,
                note: BootstrapNote::SpawnFailed(err),
            }
        }
    }
    if wait_for_socket(&rt, socket, SPAWN_READY_TIMEOUT) {
        BootstrapOutcome {
            connected: true,
            note: BootstrapNote::Spawned,
        }
    } else {
        BootstrapOutcome {
            connected: false,
            note: BootstrapNote::SpawnFailed(format!(
                "socket {} did not appear within {SPAWN_READY_TIMEOUT:?}",
                socket.display()
            )),
        }
    }
}

fn probe_socket(rt: &Runtime, socket: &Path) -> bool {
    let endpoint = endpoint_for(socket);
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_millis(250), connect(&endpoint))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
    })
}

fn wait_for_socket(rt: &Runtime, socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if probe_socket(rt, socket) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn endpoint_for(socket: &Path) -> Endpoint {
    #[cfg(unix)]
    {
        Endpoint::uds(socket)
    }
    #[cfg(not(unix))]
    {
        let name = format!(
            r"\\.\pipe\lazyagents-{}",
            socket.file_stem().and_then(|s| s.to_str()).unwrap_or("lad")
        );
        Endpoint::named_pipe(name)
    }
}

fn locate_lad() -> Option<PathBuf> {
    // Prefer a sibling binary so a dev `cargo run --bin la` finds the
    // matching `target/.../lad` without needing the user to install it.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(lad_filename());
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Otherwise rely on `$PATH`.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(lad_filename());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn lad_filename() -> &'static str {
    #[cfg(windows)]
    {
        "lad.exe"
    }
    #[cfg(not(windows))]
    {
        "lad"
    }
}

fn spawn_lad_daemonize(lad: &Path) -> Result<(), String> {
    let status = std::process::Command::new(lad)
        .arg("daemonize")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("spawn {}: {e}", lad.display()))?;
    if status.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&status.stderr);
        Err(format!(
            "{} daemonize exited {:?}: {}",
            lad.display(),
            status.status.code(),
            stderr.trim()
        ))
    }
}
