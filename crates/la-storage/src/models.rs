use base64::Engine;
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

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Cron {
    pub id: String,
    pub name: String,
    pub enabled: i64,
    pub project_id: String,
    pub backend_id: String,
    pub spawn_args: String,
    pub prompt: String,
    pub cron_expr: String,
    pub tz: String,
    pub catchup_mode: String,
    pub max_concurrent_runs: i64,
    pub max_runs_per_day: i64,
    pub max_runtime_s: i64,
    pub cost_budget_usd_per_day: Option<f64>,
    pub failure_backoff: String,
    pub pause_on_consecutive_failures: i64,
    pub consecutive_failures: i64,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    /// Keep this format for every scheduler-written timestamp so string
    /// comparisons in due queries remain chronologically correct.
    pub last_fired_at: Option<String>,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub next_fire_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CronUpsert {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub project_id: String,
    pub backend_id: String,
    pub spawn_args: serde_json::Value,
    pub prompt: String,
    pub cron_expr: String,
    pub tz: String,
    pub catchup_mode: String,
    pub max_concurrent_runs: i64,
    pub max_runs_per_day: i64,
    pub max_runtime_s: i64,
    pub cost_budget_usd_per_day: Option<f64>,
    pub failure_backoff: String,
    pub pause_on_consecutive_failures: i64,
    pub consecutive_failures: i64,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub last_fired_at: Option<String>,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub next_fire_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct RunRecord {
    pub id: String,
    pub cron_id: Option<String>,
    pub session_id: Option<String>,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub scheduled_at: String,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub started_at: Option<String>,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub finished_at: Option<String>,
    pub status: String,
    pub exit_code: Option<i64>,
    pub coalesced_count: i64,
    pub cost_usd_est: Option<f64>,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
    pub tail_log: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewRun {
    pub id: String,
    pub cron_id: Option<String>,
    pub session_id: Option<String>,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub scheduled_at: String,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub started_at: Option<String>,
    pub status: String,
    pub coalesced_count: i64,
}

/// Audit-only insert used by WEK-33 / M3.2 quota gates when a cron fire is
/// refused before any session is spawned. `finished_at` is set equal to
/// `scheduled_at` in the SQL (single statement, same caller-supplied value);
/// `started_at`, `session_id`, `exit_code`, `cost_usd_est`, and `tail_log`
/// are NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewRejectedRun<'a> {
    pub id: &'a str,
    pub cron_id: &'a str,
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub scheduled_at: &'a str,
    /// Must satisfy the `runs.status` CHECK constraint. Today:
    /// `"budget_exceeded"` for cost-budget refusals, `"cancelled"` for
    /// every other quota dimension.
    pub status: &'a str,
    pub coalesced_count: i64,
    /// Machine-parseable reason tag (e.g. `"quota_max_runs_per_day"`).
    pub error_kind: &'a str,
    /// Free-form human detail; surfaces in TUI run list.
    pub error_detail: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunFinish {
    /// UTC timestamp in SQLite lexical format: `YYYY-MM-DD HH:MM:SS`.
    pub finished_at: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub cost_usd_est: Option<f64>,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
    pub tail_log: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunsListFilter<'a> {
    pub cron_id: Option<&'a str>,
    pub since: Option<&'a str>,
    pub limit: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunsArchiveOutcome {
    pub archived_rows: u64,
    pub archive_files: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunArchiveLine {
    pub id: String,
    pub cron_id: Option<String>,
    pub session_id: Option<String>,
    pub scheduled_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub status: String,
    pub exit_code: Option<i64>,
    pub coalesced_count: i64,
    pub cost_usd_est: Option<f64>,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
    pub tail_log_base64: Option<String>,
}

impl From<RunRecord> for RunArchiveLine {
    fn from(run: RunRecord) -> Self {
        Self {
            id: run.id,
            cron_id: run.cron_id,
            session_id: run.session_id,
            scheduled_at: run.scheduled_at,
            started_at: run.started_at,
            finished_at: run.finished_at,
            status: run.status,
            exit_code: run.exit_code,
            coalesced_count: run.coalesced_count,
            cost_usd_est: run.cost_usd_est,
            error_kind: run.error_kind,
            error_detail: run.error_detail,
            tail_log_base64: run
                .tail_log
                .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes)),
        }
    }
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
    /// Absolute path to the backend's own transcript file for sessions
    /// promoted via `sessions.import` (architecture §4.2 双轨). NULL on
    /// native (`origin='user'` / `'cron:*'`) sessions; the daemon
    /// promises never to mutate this file.
    pub external_path: Option<String>,
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
    /// See [`Session::external_path`]. Set by the `sessions.import`
    /// path; left `None` on native session creation.
    pub external_path: Option<String>,
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
