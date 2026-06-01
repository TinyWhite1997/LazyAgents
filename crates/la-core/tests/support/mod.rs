//! Shared test scaffolding used by the manager / lifecycle tests.
//!
//! Brings up an in-process `Storage` against a tempdir, registers a fake
//! project + backend row, and builds a `SessionManager` with very short
//! timing knobs so the lifecycle tests don't have to sleep for the real
//! 2-second `Waiting` threshold.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{AdapterDescriptor, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec};
use la_core::{ManagerConfig, SessionManager};
use la_ipc::HubConfig;
use la_pty::PtySize;
use la_storage::{BackendUpsert, NewProject, Storage, StorageConfig};
use tempfile::TempDir;

pub const TEST_BACKEND: &str = "shtest";

/// Bundle returned by [`new_manager`] so a test can interact with the
/// manager and reach into the tempdir / storage if it needs to.
pub struct TestHarness {
    pub manager: SessionManager,
    pub storage: Storage,
    pub project_id: String,
    pub project_root: PathBuf,
    pub _tempdir: TempDir,
}

/// Build a fresh manager wired to a fresh SQLite + tempdir.
///
/// `persist_chunks = false` keeps each PTY chunk out of the
/// `session_chunks` write path so the lifecycle tests aren't bottlenecked
/// on SQLite. The lifecycle test that DOES care turns it back on.
pub async fn new_manager(persist_chunks: bool) -> TestHarness {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(StorageConfig::for_test(dir.path()))
        .await
        .expect("storage open");

    // Pick a project root inside the tempdir so cwd is real on every
    // test platform and doesn't collide with /tmp leftovers.
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root).expect("project root");

    // Seed the backend + project rows so foreign keys on `sessions`
    // resolve. The adapter's id matches `TEST_BACKEND`.
    storage
        .backends()
        .upsert(BackendUpsert {
            id: TEST_BACKEND,
            display_name: "Shell Test Backend",
            version: Some("0.0.0"),
            available: true,
        })
        .await
        .expect("backend upsert");

    let project_id = la_storage::new_id();
    storage
        .projects()
        .create(NewProject {
            id: project_id.clone(),
            root_path: project_root.to_string_lossy().into_owned(),
            display_name: "Test Project".to_string(),
            vcs: None,
        })
        .await
        .expect("project create");

    let manager = SessionManager::new(
        storage.clone(),
        ManagerConfig {
            hub: HubConfig {
                ring_bytes: 8 * 1024,
                sub_queue_bytes: 4 * 1024,
                park_duration: Duration::from_millis(200),
            },
            bus_capacity: 32,
            // Test-friendly timing: 100 ms idle → Waiting, 25 ms self-promote.
            waiting_idle: Duration::from_millis(100),
            running_promote: Duration::from_millis(25),
            initial_pty: PtySize::default(),
            persist_chunks,
        },
    );

    TestHarness {
        manager,
        storage,
        project_id,
        project_root,
        _tempdir: dir,
    }
}

/// Adapter that spawns `/bin/sh -c <script>` for tests.
///
/// We don't fake the PTY because the WEK-18 acceptance criteria require
/// real PTY lifecycle observation ("client disconnect doesn't kill the
/// child", "spawn → state sequence correct"). A real shell session is the
/// cheapest way to exercise that for tests on Linux.
pub struct ShellAdapter {
    pub script: String,
}

impl ShellAdapter {
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
        }
    }
}

#[async_trait]
impl AgentAdapter for ShellAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: TEST_BACKEND,
            display_name: "Shell Test Backend",
            default_program: "sh",
            docs_url: "https://example.test/shtest",
        }
    }

    async fn probe(&self) -> ProbeResult {
        ProbeResult::Available {
            version: "0.0.0".into(),
        }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, la_adapter::AdapterError> {
        Ok(SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), self.script.clone().into()],
            env: req.env.clone(),
            cwd: req.cwd.clone(),
            pty: req.pty,
            stdin_mode: req.stdin_mode,
        })
    }

    fn encode_user_input(&self, text: &str) -> Bytes {
        Bytes::copy_from_slice(text.as_bytes())
    }
}

/// Convenience: a spawn request with cwd pointed at the harness project root.
pub fn request_in(root: &std::path::Path) -> SpawnRequest {
    SpawnRequest::new(root.to_path_buf())
}
