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

use la_ipc::SocketDiscovery;
use la_tui::bootstrap::{bootstrap_daemon, BootstrapNote, BootstrapOutcome};
use la_tui::status::Status;
use la_tui::update_check;
use la_tui::{App, AppMsg, MockSessionSource};

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
    println!("  LAZYAGENTS_MANAGED_BY=<tag>  declared by service units (systemd/launchd/");
    println!("                               windows-task); also disables auto-daemonize");
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
    bootstrap_status.right_context = bootstrap_status_context(&bootstrap, &location.socket_path);
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
///
/// Now lives in [`la_tui::bootstrap`]; the binary keeps a thin
/// formatter (`status_context`) on top because the wire-level
/// [`BootstrapOutcome`] is library-agnostic and doesn't carry
/// status-bar copy. See module docs there.
fn bootstrap_status_context(outcome: &BootstrapOutcome, socket: &Path) -> String {
    match &outcome.note {
        BootstrapNote::AlreadyUp => format!("daemon @ {}", socket.display()),
        BootstrapNote::Spawned => format!("spawned lad @ {}", socket.display()),
        BootstrapNote::AutoSpawnDisabled => format!(
            "no daemon (LAZYAGENTS_NO_AUTODAEMON set); expected at {}",
            socket.display()
        ),
        BootstrapNote::DaemonBinaryMissing => {
            "no daemon (lad not on PATH); start with `lad daemonize`".to_string()
        }
        BootstrapNote::SpawnFailed(why) => format!("daemon spawn failed: {why}"),
        BootstrapNote::ManagedBy(tag) => {
            format!("daemon managed by {tag}; expected at {}", socket.display())
        }
    }
}
