//! `lad doctor` / `la doctor` health & dependency checklist (M4.4 /
//! WEK-74 S2).
//!
//! Covers architecture §9.3 "依赖 / 权限 / 版本" and produces three
//! tiers of exit codes the installer post-install hook branches on:
//!
//! - **0** = every critical check passed *and* every adapter is
//!   `Available`. Installer stays silent.
//! - **1** = a critical check failed (daemon / socket / state / git /
//!   version). Installer must surface the failure and bail.
//! - **2** = critical green, but at least one optional adapter is
//!   `NotInstalled` / `Unauthenticated` (degraded). Installer
//!   succeeds but prints a doctor summary.
//!
//! Symbols rendered next to each line follow the same tiering:
//!
//! - `✓` = passed
//! - `✗` = critical failure (red)
//! - `!` = degraded / optional missing (yellow)
//!
//! The module is wired pure: every check returns a [`CheckOutcome`]
//! against an explicit [`DoctorInputs`] fixture, and
//! [`run_with_inputs`] turns the list into the wire output + an
//! [`ExitTier`]. The binary glue in `src/bin/lad.rs` builds the inputs
//! from the live process; tests construct it from tempdirs / fake
//! adapters.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Duration;

use la_adapter::{AgentAdapter, ProbeResult};

/// Three-tier outcome used by every check line. Cf. module-level docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Critical or optional check passed.
    Pass,
    /// Critical check failed (drives exit code 1).
    Critical,
    /// Optional / degraded (drives exit code 2 when no Critical fired).
    Degraded,
}

impl CheckOutcome {
    fn symbol(self) -> char {
        match self {
            CheckOutcome::Pass => '✓',
            CheckOutcome::Critical => '✗',
            CheckOutcome::Degraded => '!',
        }
    }
}

/// One rendered row: `<symbol> <label>: <detail>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckLine {
    pub outcome: CheckOutcome,
    pub label: String,
    pub detail: String,
}

impl fmt::Display for CheckLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.outcome.symbol(),
            self.label,
            self.detail
        )
    }
}

/// Final exit-code tier. Maps to the process exit code in `lad.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitTier {
    /// 0 — every check green and every adapter Available.
    Ok,
    /// 2 — critical green; at least one optional check degraded.
    Degraded,
    /// 1 — at least one critical check failed.
    Critical,
}

impl ExitTier {
    pub fn code(self) -> u8 {
        match self {
            ExitTier::Ok => 0,
            ExitTier::Degraded => 2,
            ExitTier::Critical => 1,
        }
    }
}

/// Result of a full doctor run. The CLI binary renders [`Self::lines`]
/// to stdout and exits with [`Self::tier`]`.code()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub lines: Vec<CheckLine>,
    pub tier: ExitTier,
}

impl DoctorReport {
    /// Render as the user-facing block (one line per check, no
    /// trailing exit-code commentary). The exit-code tier is communicated
    /// by the process exit code itself, not by an extra line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.to_string());
            out.push('\n');
        }
        out
    }
}

/// Per-adapter inputs the doctor needs: the registered id, an async
/// `probe()` future, and the age of the previous cached probe (if any).
/// Adapters never registered with the daemon don't appear here; what
/// the user *configured* but the daemon couldn't connect to also doesn't
/// — that case falls under the "daemon not reachable" critical line.
pub struct DoctorAdapter {
    pub id: String,
    pub adapter: std::sync::Arc<dyn AgentAdapter>,
    /// Age of the most recent cached probe in the daemon's
    /// [`crate::HealthRegistry`]. `None` when no daemon is reachable
    /// or the daemon hasn't probed this adapter yet (e.g. brand-new
    /// adapter pre-first-probe).
    pub last_probe_age: Option<Duration>,
}

/// Authoritative inputs for a doctor run. Filled in by the CLI binary
/// from live process state, or by tests from a fixture.
pub struct DoctorInputs {
    pub socket_path: PathBuf,
    pub state_dir: PathBuf,
    pub server_version: String,
    /// The version reported by the running daemon over `initialize` (if
    /// reachable). Used to compare against [`Self::server_version`] to
    /// catch a client/daemon split. `None` when the daemon isn't up.
    pub running_daemon_version: Option<String>,
    /// Best-effort free space on the state-dir filesystem. `None` when
    /// the syscall fails or the platform doesn't expose it; the check
    /// degrades to writability only.
    pub state_dir_free_bytes: Option<u64>,
    pub adapters: Vec<DoctorAdapter>,
    /// Free-form `git --version` output. `None` when git isn't on PATH.
    pub git_version: Option<String>,
    /// Whether a daemon socket connection succeeded.
    pub daemon_reachable: bool,
    /// Probe outcome for each adapter, parallel to [`Self::adapters`].
    /// Pre-computed by the binary so the doctor module stays sync-only
    /// (probes are async; we run them once up front).
    pub adapter_probes: Vec<ProbeResult>,
}

/// Run every check against `inputs` and return the rendered lines +
/// final exit tier. Pure: no I/O happens here.
#[allow(clippy::vec_init_then_push)] // cfg-gated push in the middle precludes a clean vec![…] literal
pub fn run_with_inputs(inputs: &DoctorInputs) -> DoctorReport {
    let mut lines = Vec::new();

    lines.push(check_daemon_socket_reachable(
        &inputs.socket_path,
        inputs.daemon_reachable,
    ));

    #[cfg(unix)]
    {
        lines.push(check_socket_permissions(&inputs.socket_path));
    }

    lines.push(check_daemon_version_match(
        &inputs.server_version,
        inputs.running_daemon_version.as_deref(),
        inputs.daemon_reachable,
    ));

    lines.push(check_state_dir_writable(
        &inputs.state_dir,
        inputs.state_dir_free_bytes,
    ));

    lines.push(check_git_available(inputs.git_version.as_deref()));

    for (adapter, probe) in inputs.adapters.iter().zip(inputs.adapter_probes.iter()) {
        lines.push(check_adapter(adapter, probe));
    }

    let tier = classify(&lines);
    DoctorReport { lines, tier }
}

fn classify(lines: &[CheckLine]) -> ExitTier {
    let mut any_critical = false;
    let mut any_degraded = false;
    for line in lines {
        match line.outcome {
            CheckOutcome::Critical => any_critical = true,
            CheckOutcome::Degraded => any_degraded = true,
            CheckOutcome::Pass => {}
        }
    }
    if any_critical {
        ExitTier::Critical
    } else if any_degraded {
        ExitTier::Degraded
    } else {
        ExitTier::Ok
    }
}

fn check_daemon_socket_reachable(socket: &Path, reachable: bool) -> CheckLine {
    let label = "daemon socket reachable".to_string();
    if reachable {
        CheckLine {
            outcome: CheckOutcome::Pass,
            label,
            detail: socket.display().to_string(),
        }
    } else {
        CheckLine {
            outcome: CheckOutcome::Critical,
            label,
            detail: format!(
                "no daemon listening at {}; start with `lad start` or `lad daemonize`",
                socket.display()
            ),
        }
    }
}

#[cfg(unix)]
fn check_socket_permissions(socket: &Path) -> CheckLine {
    use std::os::unix::fs::MetadataExt;
    let label = "socket permissions".to_string();
    let meta = match std::fs::metadata(socket) {
        Ok(m) => m,
        Err(err) => {
            return CheckLine {
                outcome: CheckOutcome::Critical,
                label,
                detail: format!(
                    "could not stat {}: {err}; daemon may not have created it yet",
                    socket.display()
                ),
            }
        }
    };
    let mode = meta.mode() & 0o777;
    let owner_uid = meta.uid();
    let our_uid = unsafe { libc::geteuid() };
    if mode != 0o600 {
        return CheckLine {
            outcome: CheckOutcome::Critical,
            label,
            detail: format!(
                "{}: mode 0{:o} (expected 0600 — see §11.1 socket hardening)",
                socket.display(),
                mode
            ),
        };
    }
    if owner_uid != our_uid {
        return CheckLine {
            outcome: CheckOutcome::Critical,
            label,
            detail: format!(
                "{}: owned by uid {} but we are uid {}; another user owns the daemon",
                socket.display(),
                owner_uid,
                our_uid,
            ),
        };
    }
    CheckLine {
        outcome: CheckOutcome::Pass,
        label,
        detail: format!("0600 owner=uid:{owner_uid}"),
    }
}

fn check_daemon_version_match(
    client: &str,
    daemon: Option<&str>,
    daemon_reachable: bool,
) -> CheckLine {
    let label = "daemon version".to_string();
    match daemon {
        Some(d) if d == client => CheckLine {
            outcome: CheckOutcome::Pass,
            label,
            detail: format!("la {d} (matches client)"),
        },
        Some(d) => CheckLine {
            outcome: CheckOutcome::Critical,
            label,
            detail: format!(
                "daemon reports la {d} but this client is la {client}; one side is stale, \
                 restart the daemon or upgrade the client"
            ),
        },
        None if !daemon_reachable => CheckLine {
            // The "daemon socket reachable" line already raised
            // Critical; mirror Pass here to avoid double-counting the
            // same root cause. The detail still tells the user we
            // couldn't query.
            outcome: CheckOutcome::Pass,
            label,
            detail: format!("client la {client}; daemon unreachable, skipping match"),
        },
        None => CheckLine {
            // Reachable but version handshake failed: socket
            // connect()ed but the daemon either didn't speak the
            // initialize/health RPC, timed out, or isn't the lad we
            // expect (foreign listener on the path, protocol drift,
            // half-broken daemon mid-restart). Per DoD §"退出码" this
            // is a critical failure — exit 1.
            outcome: CheckOutcome::Critical,
            label,
            detail: format!(
                "client la {client}; daemon at the socket connect()s but did not return a \
                 version — protocol drift, foreign listener on the socket path, or daemon \
                 mid-restart. Restart `lad start` and re-run `la doctor`."
            ),
        },
    }
}

fn check_state_dir_writable(dir: &Path, free_bytes: Option<u64>) -> CheckLine {
    let label = "state dir writable".to_string();
    if let Err(err) = std::fs::create_dir_all(dir) {
        return CheckLine {
            outcome: CheckOutcome::Critical,
            label,
            detail: format!("cannot create {}: {err}", dir.display()),
        };
    }
    let probe = dir.join(".lazyagents-doctor-probe");
    if let Err(err) = std::fs::write(&probe, b"ok") {
        return CheckLine {
            outcome: CheckOutcome::Critical,
            label,
            detail: format!("cannot write {}: {err}", probe.display()),
        };
    }
    let _ = std::fs::remove_file(&probe);
    let detail = match free_bytes {
        Some(bytes) => format!("{} (free: {})", dir.display(), human_bytes(bytes)),
        None => dir.display().to_string(),
    };
    CheckLine {
        outcome: CheckOutcome::Pass,
        label,
        detail,
    }
}

fn check_git_available(git_version: Option<&str>) -> CheckLine {
    let label = "git available".to_string();
    match git_version {
        Some(v) => CheckLine {
            outcome: CheckOutcome::Pass,
            label,
            detail: v.trim().to_string(),
        },
        None => CheckLine {
            outcome: CheckOutcome::Critical,
            label,
            detail: "git not on PATH; required for worktree mode".to_string(),
        },
    }
}

fn check_adapter(adapter: &DoctorAdapter, probe: &ProbeResult) -> CheckLine {
    let label = format!("adapter {}", adapter.id);
    let probe_age_suffix = match adapter.last_probe_age {
        Some(d) => format!(" (last probe {} ago)", short_duration(d)),
        None => String::new(),
    };
    match probe {
        ProbeResult::Available { version } => CheckLine {
            outcome: CheckOutcome::Pass,
            label,
            detail: format!("ok version={version}{probe_age_suffix}"),
        },
        ProbeResult::NotInstalled { hint } => CheckLine {
            outcome: CheckOutcome::Degraded,
            label,
            detail: format!("NotInstalled — {hint}"),
        },
        ProbeResult::Unauthenticated { docs_url } => CheckLine {
            outcome: CheckOutcome::Degraded,
            label,
            detail: format!("Unauthenticated — see {docs_url}"),
        },
        ProbeResult::Error { detail } => CheckLine {
            outcome: CheckOutcome::Degraded,
            label,
            detail: format!("Error — {detail}"),
        },
    }
}

/// Format like `12s` / `3m` / `1h`; pick the largest non-zero unit so
/// the line stays single-glance-readable.
fn short_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 60 / 60)
    }
}

fn human_bytes(b: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if b >= GIB {
        format!("{:.1} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.0} MiB", b as f64 / MIB as f64)
    } else {
        format!("{b} bytes")
    }
}

/// Best-effort `git --version`. Returns `None` if git isn't on PATH or
/// runs and prints nothing (which would be a weird git build but not
/// worth treating as "ok").
pub fn detect_git_version() -> Option<String> {
    let out = ProcessCommand::new("git").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Best-effort free-bytes lookup. Uses `statvfs` on Unix; returns
/// `None` on Windows / unsupported platforms so the doctor degrades to
/// "writable only" instead of guessing.
#[cfg(unix)]
pub fn detect_state_dir_free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // `f_bavail` is blocks available to non-root; multiply by fragment
    // size to get bytes. Both fields are `c_ulong` / `fsblkcnt_t` —
    // upcast to u64 before the multiply to avoid 32-bit overflow on
    // i686 targets that ship with the dist matrix.
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn detect_state_dir_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Wall-clock probe of the daemon socket. Returns `true` when we can
/// connect within 250 ms (a daemon that takes longer than that to
/// `accept()` is functionally down for doctor purposes). The doctor
/// runs sync, so we drive this on a one-shot single-thread tokio
/// runtime.
pub fn probe_daemon_reachable(socket: &Path) -> bool {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    rt.block_on(async {
        let endpoint = la_ipc::transport::endpoint_for(socket);
        matches!(
            tokio::time::timeout(
                Duration::from_millis(250),
                la_ipc::transport::connect(&endpoint),
            )
            .await,
            Ok(Ok(_))
        )
    })
}

/// Synchronous probe of every adapter. Used by the doctor binary path
/// before calling [`run_with_inputs`]. We run them sequentially under a
/// single tokio runtime — total budget is small enough that parallel
/// probing wouldn't measurably improve UX and would complicate output
/// ordering.
pub fn probe_adapters_sync(adapters: &[DoctorAdapter]) -> Vec<ProbeResult> {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            return adapters
                .iter()
                .map(|_| ProbeResult::Error {
                    detail: format!("tokio runtime build failed: {err}"),
                })
                .collect()
        }
    };
    rt.block_on(async {
        let mut out = Vec::with_capacity(adapters.len());
        for a in adapters {
            // Match the daemon's own hard timeout so a hanging adapter
            // can't wedge the doctor command (`la doctor` is invoked
            // from the post-install hook and must always exit promptly).
            let probe = match tokio::time::timeout(Duration::from_secs(10), a.adapter.probe()).await
            {
                Ok(p) => p,
                Err(_) => ProbeResult::Error {
                    detail: "probe timed out after 10s".to_string(),
                },
            };
            out.push(probe);
        }
        out
    })
}

// Silence dead-code warnings when the `unix` cfg is off; the helpers
// above are referenced from the binary glue.
#[cfg(not(unix))]
const _: fn() = || {};

#[cfg(test)]
mod tests {
    use super::*;
    use la_adapter::{AdapterDescriptor, AdapterError, SpawnRequest, SpawnSpec};
    use std::sync::Arc;

    /// Test adapter — every variant of `ProbeResult` is exercised in
    /// the table tests below by inserting one of these and pairing it
    /// with the matching probe result.
    struct FakeAdapter {
        id: &'static str,
        display_name: &'static str,
        docs_url: &'static str,
    }

    #[async_trait::async_trait]
    impl AgentAdapter for FakeAdapter {
        fn descriptor(&self) -> AdapterDescriptor {
            AdapterDescriptor {
                id: self.id,
                display_name: self.display_name,
                default_program: "/bin/true",
                docs_url: self.docs_url,
            }
        }

        async fn probe(&self) -> ProbeResult {
            // Tests bypass this — they construct `adapter_probes`
            // directly and never call `probe_adapters_sync` on the
            // fake.
            ProbeResult::Available {
                version: "0.0.0".into(),
            }
        }

        fn spawn_spec(&self, _req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
            Err(AdapterError::NotInstalled {
                hint: "fake".into(),
            })
        }

        fn encode_user_input(&self, text: &str) -> bytes::Bytes {
            bytes::Bytes::copy_from_slice(text.as_bytes())
        }
    }

    fn fake_adapter(id: &'static str) -> DoctorAdapter {
        DoctorAdapter {
            id: id.to_string(),
            adapter: Arc::new(FakeAdapter {
                id,
                display_name: id,
                docs_url: "https://example.test",
            }),
            last_probe_age: Some(Duration::from_secs(12)),
        }
    }

    fn baseline_inputs(state_dir: PathBuf) -> DoctorInputs {
        DoctorInputs {
            socket_path: state_dir.join("lad-1.sock"),
            state_dir: state_dir.clone(),
            server_version: "1.0.0".into(),
            running_daemon_version: Some("1.0.0".into()),
            state_dir_free_bytes: Some(20 * 1024 * 1024 * 1024),
            adapters: vec![fake_adapter("claude")],
            git_version: Some("git version 2.45.0".into()),
            daemon_reachable: true,
            adapter_probes: vec![ProbeResult::Available {
                version: "2.1.158".into(),
            }],
        }
    }

    #[test]
    fn ok_path_exits_zero() {
        let tmp = tempfile::tempdir().unwrap();
        // Bypass the socket-perm check by skipping it: it would fail
        // for a non-existent socket file on Unix. Drop the file in by
        // hand at 0600.
        let socket = tmp.path().join("lad-1.sock");
        std::fs::write(&socket, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        inputs.socket_path = socket;
        let report = run_with_inputs(&inputs);
        assert_eq!(report.tier, ExitTier::Ok, "{}", report.render());
        assert_eq!(report.tier.code(), 0);
    }

    #[test]
    fn missing_daemon_is_critical() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        inputs.daemon_reachable = false;
        inputs.running_daemon_version = None;
        // Don't create the socket file — perms check will also fail,
        // which is expected: when the socket isn't there, both lines
        // fire Critical for the same root cause.
        let report = run_with_inputs(&inputs);
        assert_eq!(report.tier, ExitTier::Critical, "{}", report.render());
        assert_eq!(report.tier.code(), 1);
    }

    /// Regression for reviewer blocker #2 (M4.4):
    /// when the socket is reachable but the daemon doesn't return a
    /// version (handshake timed out, protocol drift, foreign listener
    /// on the path, daemon mid-restart), `daemon version` MUST be
    /// Critical — exit 1 — not silently skipped to Pass.
    ///
    /// Pre-fix code mirrored the `None` arm to Pass to avoid
    /// double-counting the socket-reachable check; but if the socket
    /// IS reachable, the Pass mask let exit 0/2 leak out and the
    /// installer wouldn't bail. We split the `None` arm on
    /// `daemon_reachable` so only the "unreachable" half mirrors Pass.
    #[test]
    fn reachable_socket_with_no_version_is_critical() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("lad-1.sock");
        std::fs::write(&socket, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        inputs.socket_path = socket;
        // The TCP/UDS connect succeeded but the version handshake
        // returned None. This is the exact failure mode the reviewer
        // called out: protocol drift / foreign listener / half-broken
        // daemon mid-restart. Must surface as Critical, exit 1.
        inputs.daemon_reachable = true;
        inputs.running_daemon_version = None;
        let report = run_with_inputs(&inputs);
        assert_eq!(report.tier, ExitTier::Critical, "{}", report.render());
        assert_eq!(report.tier.code(), 1);
        let rendered = report.render();
        assert!(
            rendered.contains("did not return a version"),
            "doctor must explain WHY the version line failed when socket is reachable: {rendered}"
        );
    }

    /// Companion to `reachable_socket_with_no_version_is_critical`:
    /// when the socket is unreachable, the version line must NOT
    /// re-raise Critical — the socket-reachable line already did so,
    /// and double-counting one root cause would confuse the post-install
    /// hook UX (two ✗ lines, same restart fix).
    #[test]
    fn unreachable_socket_keeps_version_line_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        // No socket file → reachable=false, version=None.
        inputs.daemon_reachable = false;
        inputs.running_daemon_version = None;
        let report = run_with_inputs(&inputs);
        // Critical via the socket-reachable line, not via the version
        // line. We assert by walking the lines.
        let version_line = report
            .lines
            .iter()
            .find(|l| l.label == "daemon version")
            .expect("version line present");
        assert_eq!(
            version_line.outcome,
            CheckOutcome::Pass,
            "unreachable socket must NOT also red-line `daemon version`: {}",
            version_line.detail
        );
    }

    #[test]
    fn version_mismatch_is_critical_even_when_socket_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("lad-1.sock");
        std::fs::write(&socket, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        inputs.socket_path = socket;
        inputs.running_daemon_version = Some("0.9.0".into());
        let report = run_with_inputs(&inputs);
        assert_eq!(report.tier, ExitTier::Critical, "{}", report.render());
        assert_eq!(report.tier.code(), 1);
    }

    #[test]
    fn missing_git_is_critical() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("lad-1.sock");
        std::fs::write(&socket, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        inputs.socket_path = socket;
        inputs.git_version = None;
        let report = run_with_inputs(&inputs);
        assert_eq!(report.tier, ExitTier::Critical, "{}", report.render());
        assert_eq!(report.tier.code(), 1);
    }

    #[test]
    fn not_installed_adapter_is_degraded_only() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("lad-1.sock");
        std::fs::write(&socket, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        inputs.socket_path = socket;
        inputs.adapters = vec![fake_adapter("claude"), fake_adapter("codex")];
        inputs.adapter_probes = vec![
            ProbeResult::Available {
                version: "2.1.158".into(),
            },
            ProbeResult::NotInstalled {
                hint: "not on PATH; install via https://example.test".into(),
            },
        ];
        let report = run_with_inputs(&inputs);
        assert_eq!(report.tier, ExitTier::Degraded, "{}", report.render());
        assert_eq!(report.tier.code(), 2);
        // The Degraded row should carry the install hint so the user
        // can act without re-running anything.
        assert!(
            report.render().contains("install via https://example.test"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn unauthenticated_adapter_is_degraded() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("lad-1.sock");
        std::fs::write(&socket, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut inputs = baseline_inputs(tmp.path().to_path_buf());
        inputs.socket_path = socket;
        inputs.adapter_probes = vec![ProbeResult::Unauthenticated {
            docs_url: "https://login.example".into(),
        }];
        let report = run_with_inputs(&inputs);
        assert_eq!(report.tier, ExitTier::Degraded);
    }

    #[test]
    fn render_uses_the_three_pinned_symbols() {
        // Exhaustively walk the symbol mapping; tests below depend on
        // the exact char so the post-install hook regex can match them.
        assert_eq!(CheckOutcome::Pass.symbol(), '✓');
        assert_eq!(CheckOutcome::Critical.symbol(), '✗');
        assert_eq!(CheckOutcome::Degraded.symbol(), '!');
    }

    #[test]
    fn human_bytes_picks_largest_unit() {
        assert_eq!(human_bytes(0), "0 bytes");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2 MiB");
        assert_eq!(human_bytes(20 * 1024 * 1024 * 1024), "20.0 GiB");
    }

    #[test]
    fn short_duration_rolls_units() {
        assert_eq!(short_duration(Duration::from_secs(5)), "5s");
        assert_eq!(short_duration(Duration::from_secs(120)), "2m");
        assert_eq!(short_duration(Duration::from_secs(60 * 60 * 3)), "3h");
    }
}
