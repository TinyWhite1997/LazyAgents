//! `lad daemonize` — fork-and-detach helper.
//!
//! Architecture §11.2: `lad daemonize` does `fork + setsid` so the daemon
//! survives terminal close. We can't use `nix::unistd::fork` after tokio
//! has started its worker threads (the child would inherit thread state
//! the runtime no longer owns), so this helper is invoked **before** the
//! tokio runtime is built — the bin entrypoint dispatches into it from
//! `main`.
//!
//! Implementation strategy: re-exec ourselves as `lad start` from the
//! detached child, then wait briefly for the socket to appear so the
//! caller can rely on "exit 0 ⇒ daemon is ready". This avoids the
//! double-fork rabbit hole that classic SysV daemonization needs while
//! still satisfying the contract.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum DaemonizeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon socket {0} did not appear within {1:?}")]
    SocketTimeout(PathBuf, Duration),
    #[error("daemon exited early with status {0}")]
    EarlyExit(std::process::ExitStatus),
}

/// Spawn `lad start` (or the supplied executable) as a detached child.
///
/// Blocks the caller until the socket file appears (proof the child has
/// bound the listener) OR `ready_timeout` elapses. On success the child
/// is fully detached: a new session leader, stdio redirected to
/// `/dev/null`, parent exits normally.
pub fn spawn_daemonized(
    exe: &Path,
    socket_path: &Path,
    extra_args: &[String],
    ready_timeout: Duration,
) -> Result<DaemonizeOutcome, DaemonizeError> {
    use std::process::Command;

    let mut cmd = Command::new(exe);
    cmd.arg("start").args(extra_args);
    // Detach stdio so the parent can exit without leaving the child
    // attached to its tty.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    #[cfg(unix)]
    setsid_pre_exec(&mut cmd);

    let mut child = cmd.spawn()?;
    let pid = child.id();

    // Best-effort detach: try_wait quickly, then poll for the socket
    // file. If the child exits before the socket appears we report
    // EarlyExit so the caller knows the daemonize failed for a reason
    // worth surfacing (rather than just timing out).
    let deadline = Instant::now() + ready_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(DaemonizeError::EarlyExit(status));
        }
        if socket_path.exists() && can_connect(socket_path) {
            // Don't reap the child — let it run as a session leader.
            // `drop(child)` leaves the OS handle alive; on Unix we leak
            // the libc `pid_t` deliberately, which is the correct
            // behaviour for a forked-and-detached daemon.
            std::mem::forget(child);
            return Ok(DaemonizeOutcome {
                pid,
                socket_path: socket_path.to_path_buf(),
            });
        }
        if Instant::now() >= deadline {
            // Try to clean up the early child before bailing.
            let _ = child.kill();
            let _ = child.wait();
            return Err(DaemonizeError::SocketTimeout(
                socket_path.to_path_buf(),
                ready_timeout,
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[derive(Debug, Clone)]
pub struct DaemonizeOutcome {
    /// PID of the detached daemon.
    pub pid: u32,
    /// Where it bound its listener.
    pub socket_path: PathBuf,
}

#[cfg(unix)]
fn setsid_pre_exec(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(|| {
            // Become session leader so we survive the parent's tty
            // close. Errors here are surfaced as the child's exit
            // status via the normal Command path.
            if libc_setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
unsafe fn libc_setsid() -> i64 {
    // Avoid pulling nix's `unistd` feature just for setsid.
    extern "C" {
        fn setsid() -> i32;
    }
    setsid() as i64
}

#[cfg(unix)]
fn can_connect(path: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn can_connect(_path: &Path) -> bool {
    true
}
