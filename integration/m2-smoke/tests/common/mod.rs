#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{
    AdapterDescriptor, AdapterError, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec,
};
use la_daemon::{Daemon, DaemonConfig, DaemonHandle, SocketDiscovery};
use la_ipc::transport::{connect, endpoint_for, StreamPair};
use la_ipc::{client_handshake, Connection};
use la_proto::jsonrpc::{Message, Request, RequestId, ResponseOutcome, RpcError};
use tempfile::TempDir;
use tokio::time::timeout;

pub const RPC_TIMEOUT: Duration = Duration::from_secs(10);

pub struct TestDaemon {
    pub socket: PathBuf,
    pub _handle: DaemonHandle,
    pub _join: tokio::task::JoinHandle<()>,
    pub tempdir: TempDir,
}

#[derive(Clone)]
pub struct FakeBackend {
    id: &'static str,
    display_name: &'static str,
    default_program: &'static str,
    docs_url: &'static str,
    probe: ProbeResult,
}

impl FakeBackend {
    pub fn available(id: &'static str, display_name: &'static str) -> Self {
        Self {
            id,
            display_name,
            default_program: "sh",
            docs_url: "https://example.test/backend",
            probe: ProbeResult::Available {
                version: "m2-fake-0.0.0".into(),
            },
        }
    }

    pub fn not_installed(id: &'static str, display_name: &'static str) -> Self {
        Self {
            id,
            display_name,
            default_program: id,
            docs_url: "https://example.test/install",
            probe: ProbeResult::NotInstalled {
                hint: format!("{id} missing from PATH"),
            },
        }
    }
}

#[async_trait]
impl AgentAdapter for FakeBackend {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: self.id,
            display_name: self.display_name,
            default_program: self.default_program,
            docs_url: self.docs_url,
        }
    }

    async fn probe(&self) -> ProbeResult {
        self.probe.clone()
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        // Windows: exec `ping.exe` directly. `ping -n 61 127.0.0.1` blocks
        // ~60s without needing a shell wrapper. Going straight to ping
        // avoids the daemon's `cmd /c <command-string>` shell-wrap reject
        // (la-pty::reject_shell_wrapper, PtyError::ShellWrapping) — that
        // gate trips on the m0-smoke `cmd /K echo` form too in spirit, but
        // m0-smoke uses `/K` which the rule allow-lists; here we'd need
        // `/C` to actually exit, so we sidestep cmd.exe entirely. ping.exe
        // is in `C:\Windows\System32\` and always in PATH on supported
        // Windows hosts.
        #[cfg(windows)]
        {
            return Ok(SpawnSpec {
                program: PathBuf::from("ping.exe"),
                args: vec!["-n".into(), "61".into(), "127.0.0.1".into()],
                env: req.env.clone(),
                cwd: req.cwd.clone(),
                pty: req.pty,
                stdin_mode: req.stdin_mode,
            });
        }

        #[cfg(unix)]
        {
            let script_dir = std::env::temp_dir().join("lazyagents-m2-smoke-scripts");
            std::fs::create_dir_all(&script_dir).map_err(AdapterError::SpawnFailed)?;
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let script_path = script_dir.join(format!("{}-{nonce}.sh", std::process::id()));
            std::fs::write(&script_path, "#!/bin/sh\ntrap 'exit 0' TERM\nsleep 60\n")
                .map_err(AdapterError::SpawnFailed)?;
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
                .map_err(AdapterError::SpawnFailed)?;
            Ok(SpawnSpec {
                program: script_path,
                args: Vec::new(),
                env: req.env.clone(),
                cwd: req.cwd.clone(),
                pty: req.pty,
                stdin_mode: req.stdin_mode,
            })
        }
    }

    fn encode_user_input(&self, text: &str) -> Bytes {
        Bytes::copy_from_slice(text.as_bytes())
    }
}

pub async fn bootstrap_daemon(backends: Vec<FakeBackend>) -> TestDaemon {
    let tempdir = tempfile::tempdir().expect("daemon tempdir");
    let runtime_dir = tempdir.path().join("runtime");
    let state_dir = tempdir.path().join("state");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    // On Windows `endpoint_for` derives the Named Pipe name from the
    // socket path's file stem, so two concurrent test daemons that both
    // pick `lad-1.sock` would race for the same `\\.\pipe\lazyagents-lad-1`.
    // Use a short, unique stem there. On Unix the tempdir is already
    // unique per call AND the full path goes through `bind(2)`, which
    // enforces `SUN_LEN` (104 bytes on macOS) — keep the canonical
    // `lad-1.sock` name so the path stays well under the limit.
    #[cfg(windows)]
    let stem = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "lad-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    };
    #[cfg(not(windows))]
    let stem = "lad-1";
    let socket = runtime_dir.join(format!("{stem}.sock"));

    let mut adapters: HashMap<String, Arc<dyn AgentAdapter>> = HashMap::new();
    for backend in backends {
        adapters.insert(backend.id.to_string(), Arc::new(backend));
    }

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
    TestDaemon {
        socket,
        _handle: handle,
        _join: join,
        tempdir,
    }
}

async fn wait_for_socket(socket: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if connect(&endpoint_for(socket)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon socket did not become ready at {}", socket.display());
}

pub async fn client(socket: &Path) -> Connection<StreamPair> {
    let stream = connect(&endpoint_for(socket)).await.expect("connect");
    let mut conn = Connection::new(stream);
    let info = client_handshake(
        &mut conn,
        "m2-smoke",
        "0.0.0",
        &[la_proto::PROTOCOL_VERSION],
    )
    .await
    .expect("handshake");
    assert_eq!(info.protocol_version, la_proto::PROTOCOL_VERSION);
    assert!(info.capabilities.worktree, "worktree capability missing");
    assert!(info.capabilities.diff, "diff capability missing");
    conn
}

pub async fn call<T, R>(conn: &mut Connection<StreamPair>, id: i64, method: &str, params: T) -> R
where
    T: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let req = Request::new(id, method.to_string(), &params).expect("encode request");
    conn.send(&Message::Request(req))
        .await
        .expect("send request");
    loop {
        let msg = timeout(RPC_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("eof");
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, RequestId::Num(id));
            return match resp.outcome {
                ResponseOutcome::Result(v) => serde_json::from_value(v).expect("decode result"),
                ResponseOutcome::Error(e) => panic!("rpc error for {method}: {e:?}"),
            };
        }
    }
}

pub async fn call_expect_err(
    conn: &mut Connection<StreamPair>,
    id: i64,
    method: &str,
    params: impl serde::Serialize,
) -> RpcError {
    let req = Request::new(id, method.to_string(), &params).expect("encode request");
    conn.send(&Message::Request(req))
        .await
        .expect("send request");
    loop {
        let msg = timeout(RPC_TIMEOUT, conn.recv())
            .await
            .expect("recv timeout")
            .expect("recv io")
            .expect("eof");
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, RequestId::Num(id));
            return match resp.outcome {
                ResponseOutcome::Error(e) => e,
                ResponseOutcome::Result(v) => panic!("expected rpc error, got {v:?}"),
            };
        }
    }
}

pub async fn run_git(cwd: &Path, args: &[&str]) {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_AUTHOR_NAME", "m2 tester")
        .env("GIT_AUTHOR_EMAIL", "m2@example.test")
        .env("GIT_COMMITTER_NAME", "m2 tester")
        .env("GIT_COMMITTER_EMAIL", "m2@example.test")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "safe.bareRepository")
        .env("GIT_CONFIG_VALUE_0", "all")
        .output()
        .await
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

pub async fn make_bare_project_repo() -> (TempDir, PathBuf) {
    let td = tempfile::tempdir().expect("project tempdir");
    let repo = td.path().join("project");
    run_git(td.path(), &["init", "-q", "project"]).await;
    let seed = repo.as_path();
    run_git(&seed, &["checkout", "-q", "-b", "main"]).await;
    run_git(&seed, &["config", "user.email", "m2@example.test"]).await;
    run_git(&seed, &["config", "user.name", "m2 tester"]).await;
    run_git(&seed, &["config", "commit.gpgsign", "false"]).await;
    tokio::fs::write(seed.join("agent.txt"), "base\n")
        .await
        .unwrap();
    run_git(&seed, &["add", "."]).await;
    run_git(&seed, &["commit", "-q", "-m", "seed"]).await;
    (td, repo)
}

pub async fn write_agent_change(worktree: &Path, backend: &str, suffix: &str) -> String {
    let path = "agent.txt".to_string();
    tokio::fs::write(
        worktree.join(&path),
        format!("base\n{backend} changed {suffix}\n"),
    )
    .await
    .unwrap();
    path
}

pub fn standard_backends() -> Vec<FakeBackend> {
    vec![
        FakeBackend::available("claude", "Claude Code"),
        FakeBackend::available("codex", "Codex CLI"),
        FakeBackend::available("opencode", "OpenCode"),
    ]
}
