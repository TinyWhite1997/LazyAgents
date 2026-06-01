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

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use la_ipc::transport::{connect, Endpoint};
use la_ipc::SocketDiscovery;
use la_tui::status::Status;
use la_tui::{App, AppMsg, MockSessionSource};
use tokio::runtime::Runtime;

const AUTO_DAEMON_ENV: &str = "LAZYAGENTS_NO_AUTODAEMON";
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    if let Err(e) = real_main() {
        eprintln!("la: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn real_main() -> io::Result<()> {
    let discovery = SocketDiscovery::default();
    let location = discovery.resolve();

    let bootstrap = bootstrap_daemon(&location.socket_path);

    let source = MockSessionSource::fixture();
    let mut app = App::new(source);
    app.handle(AppMsg::StatusUpdate(Status {
        daemon_online: bootstrap.connected,
        running: 2,
        next_cron_label: Some("cron pane in M3".to_string()),
        right_context: bootstrap.status_context(&location.socket_path),
    }));
    la_tui::runner::run(app)
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
