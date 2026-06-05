//! Daemon bootstrap path shared by the `la` binary and any test that
//! needs to assert the same auto-daemonize policy.
//!
//! This is the single owner of the rules architecture §3 + WEK-73 §S1
//! pin. The branch order is significant — env-driven service-manager
//! ownership takes precedence over a reachable socket so a host whose
//! socket happens to be reachable (e.g. a stale `lad` still bound, an
//! orphaned process post-crash) cannot mask the launchd / systemd /
//! Windows task hand-off the install path put in place:
//! - If `LAZYAGENTS_MANAGED_BY=<tag>` is set in the env (i.e. systemd /
//!   launchd / Windows task installed the daemon), refuse to spawn AND
//!   refuse to claim `AlreadyUp` even if the socket is reachable —
//!   the service manager owns the lifecycle and the status bar must
//!   surface that fact. Returns [`BootstrapNote::ManagedBy`].
//! - If `LAZYAGENTS_NO_AUTODAEMON` is set, return
//!   [`BootstrapNote::AutoSpawnDisabled`] (same rationale: caller has
//!   declared they want manual control regardless of socket state).
//! - If the socket is already reachable, return [`BootstrapNote::AlreadyUp`].
//! - Otherwise locate `lad` and run `lad daemonize`, then wait for the
//!   socket to appear within [`SPAWN_READY_TIMEOUT`].
//!
//! The `la` binary calls [`bootstrap_daemon`] on every startup; the
//! WEK-74 A6 macOS smoke test calls the same function so the
//! short-circuit can never regress without breaking both. Lifting the
//! code from `src/bin/la.rs` into the library was the WEK-74 reviewer
//! ask — see PR #58 review.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use la_ipc::transport::{connect, Endpoint};
use tokio::runtime::Runtime;

/// Env-var the `la` binary reads to skip its own auto-daemonize path.
/// Useful for tests that want to assert "no daemon" rendering without
/// the binary racing them by spawning one.
pub const AUTO_DAEMON_ENV: &str = "LAZYAGENTS_NO_AUTODAEMON";

/// Env-var that service units (systemd / launchd / Windows task)
/// export so any client that boots from a service-managed host knows
/// not to spawn its own `lad daemonize`. The value is opaque — we
/// only check for non-empty presence.
pub const MANAGED_BY_ENV: &str = "LAZYAGENTS_MANAGED_BY";

/// How long [`bootstrap_daemon`] waits for `lad daemonize` to expose
/// its socket. Tight enough to surface a hung spawn in CI; loose
/// enough to tolerate a cold-start on slow CI runners.
pub const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of the startup bootstrap. Carries enough info for the
/// status bar to tell the user what happened, and enough for tests to
/// assert which branch the policy took without re-parsing log strings.
#[derive(Debug)]
pub struct BootstrapOutcome {
    pub connected: bool,
    pub note: BootstrapNote,
}

/// Discriminant for [`BootstrapOutcome`]; one variant per exit edge of
/// [`bootstrap_daemon`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapNote {
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
    /// `LAZYAGENTS_MANAGED_BY=<tag>` is set: a service manager
    /// (`systemd` / `launchd` / `windows-task`) owns the daemon's
    /// lifecycle so `la` declines to auto-daemonize even if the
    /// socket isn't reachable. WEK-73 / S1.
    ManagedBy(String),
}

impl BootstrapOutcome {
    /// Convenience: did the bootstrap take a spawn-suppressing branch?
    /// Used by tests that want to assert "policy fired and we did not
    /// touch the process table" without caring which specific tag was
    /// set.
    pub fn suppressed_spawn(&self) -> bool {
        matches!(
            self.note,
            BootstrapNote::ManagedBy(_)
                | BootstrapNote::AutoSpawnDisabled
                | BootstrapNote::AlreadyUp
                | BootstrapNote::DaemonBinaryMissing
        )
    }
}

/// Run the startup bootstrap policy against `socket`. See the module
/// docs for the decision table. Public because the WEK-74 A6 macOS
/// smoke test needs to exercise the exact code path the binary takes.
pub fn bootstrap_daemon(socket: &Path) -> BootstrapOutcome {
    // Build the tokio runtime once for the whole bootstrap so the
    // `wait_for_socket` polling loop reuses one reactor instead of
    // spinning a fresh `new_current_thread` runtime every 50 ms.
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

    // S1 / WEK-73: service-manager-owned lifecycle. Checked BEFORE the
    // socket probe (WEK-74 review round 2): once an install path has
    // exported LAZYAGENTS_MANAGED_BY, this client must never claim
    // AlreadyUp / connected=true on that socket — that would let the
    // status bar lie about who owns the daemon, and a future bug that
    // breaks the socket-probe order could let auto-spawn re-enter from
    // a host whose socket happened to be reachable. LAZYAGENTS_NO_AUTODAEMON
    // is checked in the same band for symmetry: the caller has
    // declared they want manual control, full stop, regardless of
    // whatever `lad` happens to be answering on the socket right now.
    if let Some(tag) = std::env::var_os(MANAGED_BY_ENV)
        .and_then(|v| v.into_string().ok())
        .filter(|s| !s.is_empty())
    {
        return BootstrapOutcome {
            connected: false,
            note: BootstrapNote::ManagedBy(tag),
        };
    }
    if std::env::var_os(AUTO_DAEMON_ENV).is_some() {
        return BootstrapOutcome {
            connected: false,
            note: BootstrapNote::AutoSpawnDisabled,
        };
    }
    if probe_socket(&rt, socket) {
        return BootstrapOutcome {
            connected: true,
            note: BootstrapNote::AlreadyUp,
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
    let status = ProcessCommand::new(lad)
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
