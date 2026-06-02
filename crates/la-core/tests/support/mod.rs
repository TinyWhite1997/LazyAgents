//! Shared test scaffolding used by the manager / lifecycle tests.
//!
//! Brings up an in-process `Storage` against a tempdir, registers a fake
//! project + backend row, and builds a `SessionManager` with very short
//! timing knobs so the lifecycle tests don't have to sleep for the real
//! 2-second `Waiting` threshold.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{AdapterDescriptor, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec};
use la_core::{ManagerConfig, SessionManager, WorktreeManager};
use la_ipc::HubConfig;
use la_pty::PtySize;
use la_storage::{BackendUpsert, NewProject, Storage, StorageConfig};
use tempfile::TempDir;

pub const TEST_BACKEND: &str = "shtest";

/// Bundle returned by [`new_manager`] / [`harness_with_worktree`] so a
/// test can interact with the manager and reach into the tempdir /
/// storage if it needs to.
pub struct TestHarness {
    pub manager: SessionManager,
    pub storage: Storage,
    pub project_id: String,
    pub project_root: PathBuf,
    pub state_dir: PathBuf,
    pub worktree_manager: Option<Arc<WorktreeManager>>,
    pub _tempdir: TempDir,
}

impl TestHarness {
    /// Convenience: the canonical `<state_dir>/worktrees` location used
    /// by [`WorktreeManager::for_state_dir`].
    pub fn worktrees_root(&self) -> PathBuf {
        self.state_dir.join("worktrees")
    }
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
            worktree: None,
        },
    );

    TestHarness {
        manager,
        storage,
        project_id,
        project_root,
        state_dir: dir.path().to_path_buf(),
        worktree_manager: None,
        _tempdir: dir,
    }
}

/// Build a harness whose `SessionManager` has a real `WorktreeManager`
/// wired up + registers the supplied `repo_root` as the project so
/// `spawn_with_options` can be driven end-to-end. The hook timeout is
/// the production default (60 s) — use
/// [`harness_with_worktree_short_timeout`] for the `timeout` hook test
/// so it doesn't block CI for a minute.
pub async fn harness_with_worktree(repo_root: &Path) -> TestHarness {
    harness_with_worktree_inner(repo_root, false).await
}

/// Same as [`harness_with_worktree`] but with a 1 s hook timeout.
/// Required for the `HookStatus::Timeout` assertion — see
/// `tests/worktree.rs::hook_outcomes_are_persisted`.
pub async fn harness_with_worktree_short_timeout(repo_root: &Path) -> TestHarness {
    harness_with_worktree_inner(repo_root, true).await
}

async fn harness_with_worktree_inner(repo_root: &Path, short_hook_timeout: bool) -> TestHarness {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(StorageConfig::for_test(dir.path()))
        .await
        .expect("storage open");

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
            root_path: repo_root.to_string_lossy().into_owned(),
            display_name: "Test Project".to_string(),
            vcs: Some("git".to_string()),
        })
        .await
        .expect("project create");

    // The hook timeout knob lives on `WorktreeManager` for production;
    // for tests we use a separate constructor so the hook-timeout test
    // doesn't have to sit for 60 seconds.
    let wm = if short_hook_timeout {
        Arc::new(WorktreeManager::for_state_dir_with_hook_timeout(
            dir.path(),
            Duration::from_secs(1),
        ))
    } else {
        Arc::new(WorktreeManager::for_state_dir(dir.path()))
    };

    let manager = SessionManager::new(
        storage.clone(),
        ManagerConfig {
            hub: HubConfig {
                ring_bytes: 8 * 1024,
                sub_queue_bytes: 4 * 1024,
                park_duration: Duration::from_millis(200),
            },
            bus_capacity: 32,
            waiting_idle: Duration::from_millis(100),
            running_promote: Duration::from_millis(25),
            initial_pty: PtySize::default(),
            persist_chunks: false,
            worktree: Some(wm.clone()),
        },
    );

    TestHarness {
        manager,
        storage,
        project_id,
        project_root: repo_root.to_path_buf(),
        state_dir: dir.path().to_path_buf(),
        worktree_manager: Some(wm),
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
