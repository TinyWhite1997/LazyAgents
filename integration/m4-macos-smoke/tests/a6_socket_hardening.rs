//! A6 macOS socket hardening + launchd short-circuit assertions
//! (WEK-74 / M4.4). Every test in this file is cfg-gated on
//! `cfg(target_os = "macos")`; on Linux/Windows the file compiles to a
//! no-op so a workspace-wide `cargo test` still passes everywhere.
//!
//! ## Coverage map (from the WEK-74 DoD)
//!
//! 1. **Socket mode = 0600** — `stat -f '%Lp' lad-1.sock` == `600`.
//! 2. **Parent dir mode = 0700** — `stat -f '%Lp' lazyagents` == `700`.
//! 3. **Non-owner dial is refused** — re-dial from a different uid via
//!    `sudo -n -u nobody`. Two paths qualify: (a) the test is already
//!    `root` (CI's default), or (b) passwordless sudo to `nobody` is
//!    available. Otherwise the test **fails** (not silently skips) —
//!    A6 §3 is a hard DoD line and a green run that didn't exercise
//!    it would defeat the whole point. The expected failure is
//!    `ECONNREFUSED` / `EACCES`; `EPERM` is NOT accepted because the
//!    DoD pins those two exact errnos and a kernel returning `EPERM`
//!    here would indicate the hardening fired in an unexpected band.
//! 4. **launchd short-circuit (PID-stability)** — when
//!    `LAZYAGENTS_MANAGED_BY=launchd` is set, the bootstrap path must
//!    NOT spawn a second `lad`. The full launchctl bootstrap dance
//!    needs SIP-bypass / `sudo` and is the macOS Reviewer's M4.8
//!    work; here we only assert the in-process bootstrap *flag* path
//!    that owns the no-spawn invariant.

#![cfg(target_os = "macos")]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{
    AdapterDescriptor, AdapterError, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec,
};
use la_daemon::{Daemon, DaemonConfig, SocketDiscovery};
use la_ipc::transport::{connect, Endpoint};
use tempfile::TempDir;

const SOCKET_FILE_NAME: &str = "lad-1.sock";

struct FixtureBackend;

#[async_trait]
impl AgentAdapter for FixtureBackend {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "claude",
            display_name: "Claude (fixture)",
            default_program: "/usr/bin/true",
            docs_url: "https://example.test",
        }
    }

    async fn probe(&self) -> ProbeResult {
        ProbeResult::Available {
            version: "fixture".into(),
        }
    }

    fn spawn_spec(&self, _req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        Err(AdapterError::NotInstalled {
            hint: "fixture-only".into(),
        })
    }

    fn encode_user_input(&self, text: &str) -> Bytes {
        Bytes::copy_from_slice(text.as_bytes())
    }
}

/// Bring up a daemon on a tempdir-rooted socket so the test owns the
/// runtime dir + socket file. Mirrors what `m2-smoke/common::bootstrap_daemon`
/// does, but kept local here so this crate doesn't depend on m2-smoke's
/// test-only `common/` module (which lives outside the public surface).
async fn bring_up() -> (
    TempDir,
    PathBuf,
    la_daemon::DaemonHandle,
    tokio::task::JoinHandle<()>,
) {
    let tempdir = tempfile::tempdir().expect("daemon tempdir");
    let runtime_dir = tempdir.path().join("lazyagents");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    // Don't create the runtime dir — let `Daemon::bind` exercise the
    // same 0o700 chmod path the production binary would use, so this
    // test catches a regression in `ensure_runtime_dir` permissions.
    let socket = runtime_dir.join(SOCKET_FILE_NAME);

    let mut adapters: std::collections::HashMap<String, Arc<dyn AgentAdapter>> =
        std::collections::HashMap::new();
    adapters.insert("claude".to_string(), Arc::new(FixtureBackend));

    let config = DaemonConfig {
        state_dir,
        socket_discovery: SocketDiscovery::with_override(socket.clone()),
        adapters,
        probe_interval: Duration::from_millis(100),
        ..DaemonConfig::default()
    };
    let daemon = Daemon::bind(config).await.expect("bind daemon");
    let (handle, join) = daemon.spawn();
    wait_for_socket(&socket).await;
    (tempdir, socket, handle, join)
}

async fn wait_for_socket(socket: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if connect(&Endpoint::uds(socket)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket did not become ready at {}", socket.display());
}

// --- A6 §1: socket file is 0600 ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a6_socket_file_is_mode_0600() {
    let (_dir, socket, _handle, _join) = bring_up().await;
    let meta = std::fs::metadata(&socket).expect("socket metadata");
    let mode = meta.mode() & 0o777;
    assert_eq!(
        mode,
        0o600,
        "socket file {} must be 0600 (§11.1 socket hardening); got 0{:o}",
        socket.display(),
        mode,
    );
}

// --- A6 §2: parent runtime dir is 0700 --------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a6_runtime_dir_is_mode_0700() {
    let (_dir, socket, _handle, _join) = bring_up().await;
    let parent = socket.parent().expect("socket has parent dir");
    let meta = std::fs::metadata(parent).expect("runtime dir metadata");
    let mode = meta.mode() & 0o777;
    assert_eq!(
        mode,
        0o700,
        "runtime dir {} must be 0700 (la-ipc::paths::ensure_runtime_dir); got 0{:o}",
        parent.display(),
        mode,
    );
}

// --- A6 §3: non-owner uid dial is refused -----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a6_non_owner_dial_is_refused() {
    let our_uid = unsafe { libc::geteuid() };
    // A6 §3 must actually run, not silently skip. Two execution
    // modes qualify:
    //   (a) we ARE root → we can drop to `nobody` directly via sudo.
    //   (b) we are not root, but passwordless sudo to `nobody` is
    //       configured (GitHub macOS hosted runners ship with NOPASSWD
    //       sudo for the runner user) → `sudo -n -u nobody true`
    //       returns 0 and we exercise the same path.
    // If NEITHER holds we FAIL the test rather than print a skip
    // notice: macOS Reviewer round 2 flagged that silent green here
    // means A6 §3 isn't actually verified in PR CI, and the macOS
    // hosted runner does have NOPASSWD sudo so we shouldn't need an
    // escape hatch. A future host that genuinely cannot run sudo
    // should publish that fact via a CI failure, not a green run.
    if our_uid != 0 {
        let probe = Command::new("sudo")
            .arg("-n")
            .arg("-u")
            .arg("nobody")
            .arg("true")
            .output();
        let sudo_ok = matches!(&probe, Ok(out) if out.status.success());
        assert!(
            sudo_ok,
            "a6_non_owner_dial_is_refused cannot drop uids: uid={our_uid}, \
             `sudo -n -u nobody true` did not succeed (output: {:?}). \
             A6 §3 is a hard DoD line; refusing to skip silently. If this is \
             a developer machine without NOPASSWD sudo, run the test under \
             `sudo cargo test ...` or fix the sudoers config; CI macOS-14 / \
             macos-13 hosted runners ship NOPASSWD sudo by default.",
            probe.as_ref().map(|out| {
                format!(
                    "status={:?} stderr={:?}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                )
            }),
        );
    }

    let (_dir, socket, _handle, _join) = bring_up().await;
    // Compile a one-liner that tries to connect and prints the errno
    // family the syscall returned. We exec it under `sudo -u nobody`
    // so the connect happens from a uid that doesn't own the parent
    // 0700 directory; the kernel must refuse the dial.
    let py = format!(
        r#"
import socket, sys, errno
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect("{}")
    print("CONNECTED")
    sys.exit(99)
except OSError as e:
    sys.stderr.write(f"errno={{e.errno}} name={{errno.errorcode.get(e.errno, '?')}}\n")
    sys.exit(0)
"#,
        socket.display()
    );

    let output = Command::new("sudo")
        .arg("-n") // never prompt; if sudo can't run we want a failure, not a hang
        .arg("-u")
        .arg("nobody")
        .arg("python3")
        .arg("-c")
        .arg(&py)
        .output()
        .expect("spawn sudo python3");
    assert!(
        output.status.success(),
        "sudo -u nobody connect probe exited non-zero (this means the dial succeeded, \
         which violates A6 §3):\nstdout={:?}\nstderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The DoD pins this to ECONNREFUSED or EACCES — the two errnos
    // the 0600 socket / 0700 parent dir actually return on macOS.
    // EPERM is intentionally NOT in the accepted set: the round-2
    // macOS review asked whether there was architectural basis for
    // EPERM and the answer is no — if the kernel returned EPERM the
    // hardening fired in a band we didn't design for and we want
    // that to surface as a test failure, not green.
    let accepted = ["ECONNREFUSED", "EACCES"];
    assert!(
        accepted.iter().any(|name| stderr.contains(name)),
        "expected one of {accepted:?} from non-owner dial; got: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("CONNECTED"),
        "non-owner dial unexpectedly succeeded — A6 §3 regression",
    );
}

// --- A6 §4: launchd short-circuit (no double-spawn, PID stable) -------------

/// When `LAZYAGENTS_MANAGED_BY=launchd` is set, **any** code path
/// that *would* have spawned `lad daemonize` MUST skip the spawn —
/// launchd owns the lifecycle once installed and a second `lad`
/// would race the launchd-owned one on the same socket.
///
/// This test calls the **same library function** that the `la`
/// binary calls at startup — `la_tui::bootstrap::bootstrap_daemon` —
/// rather than reimplementing the policy in the test body. That's
/// the reviewer's blocker fix: an in-process `connect(socket)`
/// bypasses every short-circuit because no spawn code is on the
/// path. The library entry point IS the policy.
///
/// We assert three things:
/// 1. The bootstrap returns `ManagedBy("launchd")` — the policy
///    branch we care about fired.
/// 2. The bootstrap reports `connected=false` even though we hand
///    it the test daemon's reachable socket. (The point of the
///    short-circuit is that the binary defers to the service
///    manager; reporting AlreadyUp here would mean the env-var
///    branch took a back seat to the socket probe and a future bug
///    that breaks the socket-probe order could let auto-spawn
///    re-enter.)
/// 3. The `lad` process count is unchanged before vs. after. We
///    measure delta, not absolute, so a CI host with unrelated
///    `lad` instances doesn't flake the test.
///
/// The full launchd install→bootstrap→kickstart→doctor→stop→bootout
/// path is the macOS Reviewer's M4.8 work (real `launchctl`, real
/// `dev.lazyagents.lad.plist`); this test deliberately scopes down
/// to the env-var short-circuit the bootstrap policy encodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a6_launchd_short_circuit_does_not_spawn_second_lad() {
    let (_dir, socket, _handle, _join) = bring_up().await;

    let before = count_lad_processes();

    // Force the launchd code path. Save the prior values so we can
    // restore them before asserting, otherwise a panic mid-assert
    // would leak the env into sibling tests.
    let prev_managed = std::env::var_os("LAZYAGENTS_MANAGED_BY");
    let prev_no_autod = std::env::var_os("LAZYAGENTS_NO_AUTODAEMON");
    std::env::set_var("LAZYAGENTS_MANAGED_BY", "launchd");
    // Belt-and-braces: also clear NO_AUTODAEMON so the test exercises
    // ONLY the LAZYAGENTS_MANAGED_BY branch; if NO_AUTODAEMON were
    // also set, the bootstrap would short-circuit on it first and
    // mask a regression in the MANAGED_BY branch.
    std::env::remove_var("LAZYAGENTS_NO_AUTODAEMON");

    // The CRITICAL move: drive the library bootstrap, NOT a bare
    // `connect`. This is the function `src/bin/la.rs` calls on every
    // startup. If `la status` / `la doctor` / the TUI ever re-grow a
    // spawn path that ignores LAZYAGENTS_MANAGED_BY, this assertion
    // catches it.
    let outcome = tokio::task::spawn_blocking({
        let s = socket.clone();
        move || la_tui::bootstrap::bootstrap_daemon(&s)
    })
    .await
    .expect("bootstrap_daemon task");

    // Give any wayward spawn a chance to surface in pgrep. 200 ms is
    // generous — `lad daemonize` would race-spawn within tens of ms
    // on macOS once it forked.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let after = count_lad_processes();

    // Restore env BEFORE the assertions so a failure doesn't leak.
    match prev_managed {
        Some(v) => std::env::set_var("LAZYAGENTS_MANAGED_BY", v),
        None => std::env::remove_var("LAZYAGENTS_MANAGED_BY"),
    }
    if let Some(v) = prev_no_autod {
        std::env::set_var("LAZYAGENTS_NO_AUTODAEMON", v);
    }

    assert!(
        matches!(
            outcome.note,
            la_tui::bootstrap::BootstrapNote::ManagedBy(ref tag) if tag == "launchd"
        ),
        "bootstrap_daemon must short-circuit on LAZYAGENTS_MANAGED_BY=launchd; got {:?}",
        outcome.note,
    );
    assert!(
        !outcome.connected,
        "even when the socket is reachable, the MANAGED_BY branch must surface connected=false \
         so the status bar tells the user the service manager owns lifecycle; got connected=true \
         which means the env-var short-circuit was reached only because the socket probe failed, \
         not because the policy fired. That hides regressions where MANAGED_BY would silently \
         be ignored on a host whose socket happens to be down.",
    );
    assert_eq!(
        after,
        before,
        "LAZYAGENTS_MANAGED_BY=launchd must NOT cause any extra lad process to appear; \
         observed lad-process count went from {before} to {after} (delta = {})",
        after as isize - before as isize,
    );
}

/// Companion to `a6_launchd_short_circuit_does_not_spawn_second_lad`:
/// the SAME library bootstrap with `LAZYAGENTS_MANAGED_BY` unset
/// and the socket already up MUST report `AlreadyUp` + connected=true
/// + no extra lad spawn. This is the "happy path" that proves the
/// env-var branch above isn't accidentally returning ManagedBy when
/// the env is empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a6_bootstrap_with_reachable_socket_and_no_env_reports_already_up() {
    let (_dir, socket, _handle, _join) = bring_up().await;

    let before = count_lad_processes();
    // Be defensive: a developer machine that has the var set in its
    // shell would otherwise poison this test. We save / restore.
    let prev_managed = std::env::var_os("LAZYAGENTS_MANAGED_BY");
    let prev_no_autod = std::env::var_os("LAZYAGENTS_NO_AUTODAEMON");
    std::env::remove_var("LAZYAGENTS_MANAGED_BY");
    std::env::remove_var("LAZYAGENTS_NO_AUTODAEMON");

    let outcome = tokio::task::spawn_blocking({
        let s = socket.clone();
        move || la_tui::bootstrap::bootstrap_daemon(&s)
    })
    .await
    .expect("bootstrap_daemon task");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = count_lad_processes();

    if let Some(v) = prev_managed {
        std::env::set_var("LAZYAGENTS_MANAGED_BY", v);
    }
    if let Some(v) = prev_no_autod {
        std::env::set_var("LAZYAGENTS_NO_AUTODAEMON", v);
    }

    assert!(
        matches!(outcome.note, la_tui::bootstrap::BootstrapNote::AlreadyUp),
        "expected AlreadyUp with reachable socket and no env, got {:?}",
        outcome.note,
    );
    assert!(outcome.connected, "AlreadyUp must imply connected=true");
    assert_eq!(
        after, before,
        "AlreadyUp must short-circuit before any spawn; observed lad count {before} → {after}",
    );
}

fn count_lad_processes() -> usize {
    // `pgrep -f` matches the whole command line; the pattern matches
    // either `…/lad ` (with args) or `…/lad` (no args, end of line).
    // Use `pgrep -c` to get a count without parsing pids ourselves.
    let out = Command::new("pgrep")
        .arg("-c")
        .arg("-f")
        .arg(r"^.*/lad( |$)")
        .output();
    match out {
        Ok(out) => {
            // pgrep returns exit code 1 when there are no matches; the
            // stdout is the count either way.
            let text = String::from_utf8_lossy(&out.stdout);
            text.trim().parse::<usize>().unwrap_or(0)
        }
        Err(err) => {
            panic!("pgrep -c -f '^.*/lad( |$)' failed: {err}");
        }
    }
}
