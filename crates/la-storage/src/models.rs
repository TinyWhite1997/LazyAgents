use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Backend {
    pub id: String,
    pub display_name: String,
    pub version: Option<String>,
    pub available: i64,
    pub last_probed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendUpsert<'a> {
    pub id: &'a str,
    pub display_name: &'a str,
    pub version: Option<&'a str>,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Project {
    pub id: String,
    pub root_path: String,
    pub display_name: String,
    pub vcs: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    pub id: String,
    pub root_path: String,
    pub display_name: String,
    pub vcs: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub backend_id: String,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub state: String,
    pub exit_code: Option<i64>,
    pub pid: Option<i64>,
    pub worktree_path: Option<String>,
    pub worktree_branch: Option<String>,
    pub base_branch: Option<String>,
    pub spawn_args: String,
    pub origin: String,
    pub transcript_path: Option<String>,
    pub transcript_bytes: i64,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    /// Outcome of the optional `.lazyagents/hooks/post-create.sh` script
    /// run once after `git worktree add` succeeds. NULL on rows created
    /// before migration 0002 and on sessions that didn't take the
    /// worktree path. Per WEK-8 brief amendment R4, hook failure does
    /// NOT roll back the worktree or mutate `SessionStatus` — the TUI
    /// renders this as a separate badge.
    pub post_create_hook_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub id: String,
    pub project_id: String,
    pub backend_id: String,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub state: String,
    pub pid: Option<i64>,
    pub worktree_path: Option<String>,
    pub worktree_branch: Option<String>,
    pub base_branch: Option<String>,
    pub spawn_args: serde_json::Value,
    pub origin: String,
    /// See [`Session::post_create_hook_status`]. Optional on the create
    /// path because the worktree hook only runs *after* the row is
    /// inserted; `WorktreeManager` calls
    /// [`super::SessionsRepo::set_post_create_hook_status`] to fill it in.
    pub post_create_hook_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SessionChunk {
    pub session_id: String,
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    Stdout,
    Stderr,
    Input,
    Event,
}

impl ChunkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Input => "input",
            Self::Event => "event",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendOutcome {
    StoredInDb { seq: i64 },
    SpilledToFile { seq: i64, path: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SpillLine {
    pub session_id: String,
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub data_base64: String,
}
