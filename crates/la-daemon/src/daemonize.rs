//! `lad daemonize` — fork-and-detach helper.
//!
//! Architecture §11.2: `lad daemonize` puts the daemon into a state where
//! it survives the parent terminal closing. The implementation differs
//! between Unix and Windows in important ways:
//!
//! * **Unix** — `fork + setsid` so the child becomes a new session
//!   leader. We do this *before* the tokio runtime boots because
//!   `fork()` after tokio has spawned worker threads leaves the child
//!   holding references to runtime state the runtime no longer owns.
//!   The bin entrypoint dispatches into [`spawn_daemonized`] from
//!   `main` for that reason.
//!
//! * **Windows** — Win32 has no `fork`; we spawn `lad start` as a fresh
//!   process with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` so the
//!   new process has no console and is independent of the parent's job
//!   object. Readiness is gated on the named-pipe listener becoming
//!   connectable, which matches the contract the Unix side gives the
//!   caller ("exit 0 ⇒ daemon is ready").
//!
//! Both paths return [`DaemonizeOutcome`] when the daemon is reachable
//! within `ready_timeout`, [`DaemonizeError::SocketTimeout`] otherwise,
//! and [`DaemonizeError::EarlyExit`] if the spawned child exited before
//! the listener bound. There are NO `cfg(not(unix))` empty stubs — A8
//! requires the Windows path to be a real, working code path.

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
/// Blocks the caller until the daemon's IPC endpoint becomes
/// connectable (proof the child has bound the listener) OR
/// `ready_timeout` elapses. On success the child is fully detached:
/// new session leader on Unix, no console + own process group on
/// Windows, stdio redirected to the OS null device, parent exits
/// normally.
pub fn spawn_daemonized(
    exe: &Path,
    socket_path: &Path,
    extra_args: &[String],
    ready_timeout: Duration,
) -> Result<DaemonizeOutcome, DaemonizeError> {
    use std::process::Command;

    let mut cmd = Command::new(exe);
    cmd.arg("start").args(extra_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    #[cfg(unix)]
    setsid_pre_exec(&mut cmd);

    #[cfg(windows)]
    apply_detached_flags(&mut cmd);

    let mut child = cmd.spawn()?;
    let pid = child.id();

    // Best-effort detach: try_wait quickly, then poll for the listener
    // becoming connectable. If the child exits before the listener
    // appears we report EarlyExit so the caller knows the daemonize
    // failed for a real reason (rather than just timing out).
    let deadline = Instant::now() + ready_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(DaemonizeError::EarlyExit(status));
        }
        if endpoint_reachable(socket_path) {
            // Don't reap the child — let it run as a session leader (or
            // detached Win32 process). `mem::forget` releases the
            // platform handle without sending a SIGTERM/TerminateProcess.
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
            if libc_setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
unsafe fn libc_setsid() -> i64 {
    extern "C" {
        fn setsid() -> i32;
    }
    setsid() as i64
}

/// On Unix the "is it ready?" probe is a connect on the UDS path.
#[cfg(unix)]
fn endpoint_reachable(path: &Path) -> bool {
    use std::os::unix::net::UnixStream;
    UnixStream::connect(path).is_ok()
}

// ---------------- Windows ----------------

#[cfg(windows)]
fn apply_detached_flags(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt as _;
    // DETACHED_PROCESS (0x00000008) — child has no inherited console.
    // CREATE_NEW_PROCESS_GROUP (0x00000200) — child is the root of a new
    //   process group; it does NOT receive Ctrl-C/Ctrl-Break sent to
    //   the parent.
    // CREATE_NO_WINDOW (0x08000000) — belt-and-braces for GUI parents.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Translate the Unix-style socket path we get from
/// `SocketDiscovery::resolve()` into the Win32 named-pipe name the
/// daemon binds — mirrors `la_ipc::transport::endpoint_for`'s
/// `\\.\pipe\lazyagents-<stem>` convention, which is the listener's
/// single source of truth.
#[cfg(windows)]
fn pipe_name_from_socket_path(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("lad");
    format!(r"\\.\pipe\lazyagents-{stem}")
}

/// On Windows the "is it ready?" probe is a `WaitNamedPipeW` against
/// the daemon's named pipe. Returning true means a client could
/// connect right now.
#[cfg(windows)]
fn endpoint_reachable(socket_path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt as _;

    let pipe_name = pipe_name_from_socket_path(socket_path);
    let wide: Vec<u16> = std::ffi::OsStr::new(&pipe_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // Use a tiny timeout so the polling loop stays responsive. Returns
    // immediately if no server is listening yet (`ERROR_FILE_NOT_FOUND`)
    // or if the wait elapses; both map to "not ready".
    unsafe { wait_named_pipe(wide.as_ptr(), 50) }
}

#[cfg(windows)]
unsafe fn wait_named_pipe(pipe_name: *const u16, timeout_ms: u32) -> bool {
    extern "system" {
        fn WaitNamedPipeW(lpNamedPipeName: *const u16, nTimeOut: u32) -> i32;
    }
    WaitNamedPipeW(pipe_name, timeout_ms) != 0
}

/// Cleanup hook the daemon's bind path calls when it removes its named
/// pipe at shutdown. On Unix this is a no-op (the daemon already
/// unlinks the socket file). On Windows there's nothing to remove
/// either — named pipes vanish when the last server handle closes —
/// but the symmetric API keeps the call sites tidy.
pub fn cleanup_endpoint_artifacts(_socket_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Apply per-pipe ACLs at server-side creation time. On Windows the
/// `la_ipc::transport::Listener::bind` / `accept` paths now build an
/// owner-only SDDL (`D:P(A;;GA;;;<owner-sid>)(A;;GA;;;SY)`) and call
/// `tokio::net::windows::named_pipe::ServerOptions::
/// create_with_security_attributes_raw` for every server instance, so this
/// hook is intentionally a no-op — the transport layer owns the DACL. It
/// stays here so a future change that needs daemon-side ACL customisation
/// can plug in without touching every call site. Returning `Ok(())` on
/// Unix keeps the surface uniform across platforms.
pub fn enforce_named_pipe_acl_defaults() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn pipe_name_uses_file_stem() {
        let p = Path::new(r"C:\Users\alice\AppData\Local\lazyagents\lad-1.sock");
        assert_eq!(pipe_name_from_socket_path(p), r"\\.\pipe\lazyagents-lad-1");
    }

    #[test]
    fn cleanup_endpoint_artifacts_is_noop_for_missing_path() {
        let tmp = std::env::temp_dir().join("nonexistent-lazyagents-pipe");
        assert!(cleanup_endpoint_artifacts(&tmp).is_ok());
    }
}
