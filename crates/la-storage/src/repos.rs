use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::sqlite::SqliteQueryResult;
use sqlx::{Error as SqlxError, Sqlite, Transaction};
use tokio::io::AsyncWriteExt;

use crate::models::{
    AppendOutcome, Backend, BackendUpsert, ChunkKind, Cron, CronUpsert, NewProject, NewRun,
    NewSession, Project, RunArchiveLine, RunFinish, RunRecord, RunsArchiveOutcome, RunsListFilter,
    Session, SessionChunk, SpillLine,
};
use crate::{Result, Storage, StorageError};

pub struct BackendsRepo<'a> {
    storage: &'a Storage,
}

impl<'a> BackendsRepo<'a> {
    pub(crate) fn new(storage: &'a Storage) -> Self {
        Self { storage }
    }

    pub async fn upsert(&self, backend: BackendUpsert<'_>) -> Result<()> {
        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO backends(id, display_name, version, available, last_probed_at)
                VALUES (?1, ?2, ?3, ?4, datetime('now'))
                ON CONFLICT(id) DO UPDATE SET
                    display_name = excluded.display_name,
                    version = excluded.version,
                    available = excluded.available,
                    last_probed_at = datetime('now')
                "#,
            )
            .bind(backend.id)
            .bind(backend.display_name)
            .bind(backend.version)
            .bind(if backend.available { 1_i64 } else { 0_i64 })
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Backend>> {
        sqlx::query_as::<_, Backend>(
            "SELECT id, display_name, version, available, last_probed_at FROM backends WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    pub async fn list(&self) -> Result<Vec<Backend>> {
        sqlx::query_as::<_, Backend>(
            "SELECT id, display_name, version, available, last_probed_at FROM backends ORDER BY id",
        )
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }
}

pub struct ProjectsRepo<'a> {
    storage: &'a Storage,
}

impl<'a> ProjectsRepo<'a> {
    pub(crate) fn new(storage: &'a Storage) -> Self {
        Self { storage }
    }

    pub async fn create(&self, project: NewProject) -> Result<Project> {
        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO projects(id, root_path, display_name, vcs)
                VALUES (?1, ?2, ?3, ?4)
                "#,
            )
            .bind(&project.id)
            .bind(&project.root_path)
            .bind(&project.display_name)
            .bind(&project.vcs)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        self.get(&project.id)
            .await?
            .ok_or(StorageError::MissingProject(project.id))
    }

    pub async fn get(&self, id: &str) -> Result<Option<Project>> {
        sqlx::query_as::<_, Project>(
            "SELECT id, root_path, display_name, vcs, created_at FROM projects WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    pub async fn get_by_root_path(&self, root_path: &str) -> Result<Option<Project>> {
        sqlx::query_as::<_, Project>(
            "SELECT id, root_path, display_name, vcs, created_at FROM projects WHERE root_path = ?1",
        )
        .bind(root_path)
        .fetch_optional(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    pub async fn list(&self) -> Result<Vec<Project>> {
        sqlx::query_as::<_, Project>(
            "SELECT id, root_path, display_name, vcs, created_at FROM projects ORDER BY root_path",
        )
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    pub async fn update_display_name(&self, id: &str, display_name: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("UPDATE projects SET display_name = ?2 WHERE id = ?1")
                .bind(id)
                .bind(display_name)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("DELETE FROM projects WHERE id = ?1")
                .bind(id)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

pub struct CronsRepo<'a> {
    storage: &'a Storage,
}

impl<'a> CronsRepo<'a> {
    pub(crate) fn new(storage: &'a Storage) -> Self {
        Self { storage }
    }

    pub async fn upsert(&self, cron: CronUpsert) -> Result<Cron> {
        let spawn_args = serde_json::to_string(&cron.spawn_args)?;
        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO crons(
                    id, name, enabled, project_id, backend_id, spawn_args, prompt,
                    cron_expr, tz, catchup_mode, max_concurrent_runs, max_runs_per_day,
                    max_runtime_s, cost_budget_usd_per_day, failure_backoff,
                    pause_on_consecutive_failures, consecutive_failures, last_fired_at,
                    next_fire_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    enabled = excluded.enabled,
                    project_id = excluded.project_id,
                    backend_id = excluded.backend_id,
                    spawn_args = excluded.spawn_args,
                    prompt = excluded.prompt,
                    cron_expr = excluded.cron_expr,
                    tz = excluded.tz,
                    catchup_mode = excluded.catchup_mode,
                    max_concurrent_runs = excluded.max_concurrent_runs,
                    max_runs_per_day = excluded.max_runs_per_day,
                    max_runtime_s = excluded.max_runtime_s,
                    cost_budget_usd_per_day = excluded.cost_budget_usd_per_day,
                    failure_backoff = excluded.failure_backoff,
                    pause_on_consecutive_failures = excluded.pause_on_consecutive_failures,
                    consecutive_failures = excluded.consecutive_failures,
                    last_fired_at = excluded.last_fired_at,
                    next_fire_at = excluded.next_fire_at,
                    updated_at = datetime('now')
                "#,
            )
            .bind(&cron.id)
            .bind(&cron.name)
            .bind(if cron.enabled { 1_i64 } else { 0_i64 })
            .bind(&cron.project_id)
            .bind(&cron.backend_id)
            .bind(&spawn_args)
            .bind(&cron.prompt)
            .bind(&cron.cron_expr)
            .bind(&cron.tz)
            .bind(&cron.catchup_mode)
            .bind(cron.max_concurrent_runs)
            .bind(cron.max_runs_per_day)
            .bind(cron.max_runtime_s)
            .bind(cron.cost_budget_usd_per_day)
            .bind(&cron.failure_backoff)
            .bind(cron.pause_on_consecutive_failures)
            .bind(cron.consecutive_failures)
            .bind(&cron.last_fired_at)
            .bind(&cron.next_fire_at)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        self.get(&cron.id)
            .await?
            .ok_or(StorageError::MissingCron(cron.id))
    }

    pub async fn get(&self, id: &str) -> Result<Option<Cron>> {
        sqlx::query_as::<_, Cron>(CRON_SELECT_BY_ID)
            .bind(id)
            .fetch_optional(self.storage.reader_pool())
            .await
            .map_err(Into::into)
    }

    pub async fn list(&self) -> Result<Vec<Cron>> {
        sqlx::query_as::<_, Cron>(
            r#"
            SELECT id, name, enabled, project_id, backend_id, spawn_args, prompt,
                   cron_expr, tz, catchup_mode, max_concurrent_runs, max_runs_per_day,
                   max_runtime_s, cost_budget_usd_per_day, failure_backoff,
                   pause_on_consecutive_failures, consecutive_failures, last_fired_at,
                   next_fire_at, created_at, updated_at
            FROM crons
            ORDER BY name, id
            "#,
        )
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    pub async fn list_enabled_due(&self, now: &str) -> Result<Vec<Cron>> {
        sqlx::query_as::<_, Cron>(
            r#"
            SELECT id, name, enabled, project_id, backend_id, spawn_args, prompt,
                   cron_expr, tz, catchup_mode, max_concurrent_runs, max_runs_per_day,
                   max_runtime_s, cost_budget_usd_per_day, failure_backoff,
                   pause_on_consecutive_failures, consecutive_failures, last_fired_at,
                   next_fire_at, created_at, updated_at
            FROM crons
            WHERE enabled = 1 AND next_fire_at IS NOT NULL AND next_fire_at <= ?1
            ORDER BY next_fire_at, id
            "#,
        )
        .bind(now)
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    pub async fn mark_fired(
        &self,
        id: &str,
        last_fired_at: &str,
        next_fire_at: Option<&str>,
    ) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query(
                r#"
                UPDATE crons
                SET last_fired_at = ?2, next_fire_at = ?3, updated_at = datetime('now')
                WHERE id = ?1
                "#,
            )
            .bind(id)
            .bind(last_fired_at)
            .bind(next_fire_at)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("UPDATE crons SET enabled = ?2, updated_at = datetime('now') WHERE id = ?1")
                .bind(id)
                .bind(if enabled { 1_i64 } else { 0_i64 })
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("DELETE FROM crons WHERE id = ?1")
                .bind(id)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

pub struct RunsRepo<'a> {
    storage: &'a Storage,
}

impl<'a> RunsRepo<'a> {
    pub(crate) fn new(storage: &'a Storage) -> Self {
        Self { storage }
    }

    pub async fn create(&self, run: NewRun) -> Result<RunRecord> {
        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO runs(
                    id, cron_id, session_id, scheduled_at, started_at, status,
                    coalesced_count
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
            )
            .bind(&run.id)
            .bind(&run.cron_id)
            .bind(&run.session_id)
            .bind(&run.scheduled_at)
            .bind(&run.started_at)
            .bind(&run.status)
            .bind(run.coalesced_count)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        self.get(&run.id)
            .await?
            .ok_or(StorageError::MissingRun(run.id))
    }

    pub async fn get(&self, id: &str) -> Result<Option<RunRecord>> {
        sqlx::query_as::<_, RunRecord>(RUN_SELECT_BY_ID)
            .bind(id)
            .fetch_optional(self.storage.reader_pool())
            .await
            .map_err(Into::into)
    }

    pub async fn list(&self, filter: RunsListFilter<'_>) -> Result<Vec<RunRecord>> {
        let limit = filter.limit.clamp(1, 500);
        match (filter.cron_id, filter.since) {
            (Some(cron_id), Some(since)) => sqlx::query_as::<_, RunRecord>(
                r#"
                    SELECT id, cron_id, session_id, scheduled_at, started_at, finished_at,
                           status, exit_code, coalesced_count, cost_usd_est, error_kind,
                           error_detail, tail_log
                    FROM runs
                    WHERE cron_id = ?1 AND scheduled_at >= ?2
                    ORDER BY scheduled_at DESC, id DESC
                    LIMIT ?3
                    "#,
            )
            .bind(cron_id)
            .bind(since)
            .bind(limit)
            .fetch_all(self.storage.reader_pool())
            .await
            .map_err(Into::into),
            (Some(cron_id), None) => sqlx::query_as::<_, RunRecord>(
                r#"
                    SELECT id, cron_id, session_id, scheduled_at, started_at, finished_at,
                           status, exit_code, coalesced_count, cost_usd_est, error_kind,
                           error_detail, tail_log
                    FROM runs
                    WHERE cron_id = ?1
                    ORDER BY scheduled_at DESC, id DESC
                    LIMIT ?2
                    "#,
            )
            .bind(cron_id)
            .bind(limit)
            .fetch_all(self.storage.reader_pool())
            .await
            .map_err(Into::into),
            (None, Some(since)) => sqlx::query_as::<_, RunRecord>(
                r#"
                    SELECT id, cron_id, session_id, scheduled_at, started_at, finished_at,
                           status, exit_code, coalesced_count, cost_usd_est, error_kind,
                           error_detail, tail_log
                    FROM runs
                    WHERE scheduled_at >= ?1
                    ORDER BY scheduled_at DESC, id DESC
                    LIMIT ?2
                    "#,
            )
            .bind(since)
            .bind(limit)
            .fetch_all(self.storage.reader_pool())
            .await
            .map_err(Into::into),
            (None, None) => sqlx::query_as::<_, RunRecord>(
                r#"
                    SELECT id, cron_id, session_id, scheduled_at, started_at, finished_at,
                           status, exit_code, coalesced_count, cost_usd_est, error_kind,
                           error_detail, tail_log
                    FROM runs
                    ORDER BY scheduled_at DESC, id DESC
                    LIMIT ?1
                    "#,
            )
            .bind(limit)
            .fetch_all(self.storage.reader_pool())
            .await
            .map_err(Into::into),
        }
    }

    pub async fn attach_session(&self, id: &str, session_id: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query(
                r#"
                UPDATE runs
                SET session_id = ?2
                WHERE id = ?1
                  AND finished_at IS NULL
                  AND (session_id IS NULL OR session_id = ?2)
                "#,
            )
            .bind(id)
            .bind(session_id)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_status(&self, id: &str, status: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("UPDATE runs SET status = ?2 WHERE id = ?1 AND finished_at IS NULL")
                .bind(id)
                .bind(status)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn finish(&self, id: &str, finish: RunFinish) -> Result<bool> {
        let tail_log = finish.tail_log.map(|mut bytes| {
            if bytes.len() > 64 * 1024 {
                bytes = bytes.split_off(bytes.len() - 64 * 1024);
            }
            bytes
        });
        let result = retry_busy(|| async {
            sqlx::query(
                r#"
                UPDATE runs
                SET finished_at = ?2,
                    status = ?3,
                    exit_code = ?4,
                    cost_usd_est = ?5,
                    error_kind = ?6,
                    error_detail = ?7,
                    tail_log = ?8
                WHERE id = ?1
                  AND (finished_at IS NULL OR (finished_at = ?2 AND status = ?3))
                "#,
            )
            .bind(id)
            .bind(&finish.finished_at)
            .bind(&finish.status)
            .bind(finish.exit_code)
            .bind(finish.cost_usd_est)
            .bind(&finish.error_kind)
            .bind(&finish.error_detail)
            .bind(&tail_log)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn archive_older_than_days(&self, retention_days: i64) -> Result<RunsArchiveOutcome> {
        let modifier = format!("-{} days", retention_days.max(1));
        let rows = sqlx::query_as::<_, RunRecord>(
            r#"
            SELECT id, cron_id, session_id, scheduled_at, started_at, finished_at,
                   status, exit_code, coalesced_count, cost_usd_est, error_kind,
                   error_detail, tail_log
            FROM runs
            WHERE scheduled_at < datetime('now', ?1)
            ORDER BY scheduled_at, id
            "#,
        )
        .bind(&modifier)
        .fetch_all(self.storage.reader_pool())
        .await?;

        if rows.is_empty() {
            return Ok(RunsArchiveOutcome {
                archived_rows: 0,
                archive_files: 0,
            });
        }

        let mut by_month: BTreeMap<String, Vec<RunRecord>> = BTreeMap::new();
        for row in rows {
            by_month
                .entry(archive_month_key(&row.scheduled_at))
                .or_default()
                .push(row);
        }

        let archive_dir = self.storage.data_dir().join("runs").join("archive");
        tokio::fs::create_dir_all(&archive_dir).await?;

        let mut archived_ids = Vec::new();
        for (month, rows) in by_month.iter() {
            let path = archive_dir.join(format!("{month}.jsonl.zst"));
            append_runs_archive(&path, rows).await?;
            archived_ids.extend(rows.iter().map(|row| row.id.clone()));
        }

        let mut tx = self.storage.writer_pool().begin().await?;
        for id in &archived_ids {
            sqlx::query("DELETE FROM runs WHERE id = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        Ok(RunsArchiveOutcome {
            archived_rows: archived_ids.len() as u64,
            archive_files: by_month.len(),
        })
    }
}

pub struct SessionsRepo<'a> {
    storage: &'a Storage,
}

impl<'a> SessionsRepo<'a> {
    pub(crate) fn new(storage: &'a Storage) -> Self {
        Self { storage }
    }

    pub async fn create(&self, session: NewSession) -> Result<Session> {
        let spawn_args = serde_json::to_string(&session.spawn_args)?;
        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO sessions(
                    id, project_id, backend_id, external_id, title, state, pid,
                    worktree_path, worktree_branch, base_branch, spawn_args, origin,
                    post_create_hook_status, external_path
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                "#,
            )
            .bind(&session.id)
            .bind(&session.project_id)
            .bind(&session.backend_id)
            .bind(&session.external_id)
            .bind(&session.title)
            .bind(&session.state)
            .bind(session.pid)
            .bind(&session.worktree_path)
            .bind(&session.worktree_branch)
            .bind(&session.base_branch)
            .bind(&spawn_args)
            .bind(&session.origin)
            .bind(&session.post_create_hook_status)
            .bind(&session.external_path)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        self.get(&session.id)
            .await?
            .ok_or(StorageError::MissingSession(session.id))
    }

    pub async fn get(&self, id: &str) -> Result<Option<Session>> {
        sqlx::query_as::<_, Session>(
            r#"
            SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                   worktree_path, worktree_branch, base_branch, spawn_args, origin,
                   transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                   post_create_hook_status, external_path
            FROM sessions
            WHERE id = ?1
            "#,
        )
        .bind(id)
        .fetch_optional(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    pub async fn list_by_project(
        &self,
        project_id: &str,
        include_archived: bool,
    ) -> Result<Vec<Session>> {
        if include_archived {
            sqlx::query_as::<_, Session>(
                r#"
                SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                       worktree_path, worktree_branch, base_branch, spawn_args, origin,
                       transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                       post_create_hook_status, external_path
                FROM sessions
                WHERE project_id = ?1
                ORDER BY created_at DESC
                "#,
            )
            .bind(project_id)
            .fetch_all(self.storage.reader_pool())
            .await
            .map_err(Into::into)
        } else {
            sqlx::query_as::<_, Session>(
                r#"
                SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                       worktree_path, worktree_branch, base_branch, spawn_args, origin,
                       transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                       post_create_hook_status, external_path
                FROM sessions
                WHERE project_id = ?1 AND archived_at IS NULL
                ORDER BY created_at DESC
                "#,
            )
            .bind(project_id)
            .fetch_all(self.storage.reader_pool())
            .await
            .map_err(Into::into)
        }
    }

    pub async fn update_state(
        &self,
        id: &str,
        state: &str,
        exit_code: Option<i64>,
    ) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query(
                "UPDATE sessions SET state = ?2, exit_code = ?3, updated_at = datetime('now') WHERE id = ?1",
            )
            .bind(id)
            .bind(state)
            .bind(exit_code)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update the OS pid recorded for a session row.
    ///
    /// Called by `la-core` once `la-pty` returns the spawned child's pid.
    /// Keeping this on the repo (instead of a raw `sqlx::query` in core)
    /// preserves the "all SQL lives in la-storage" invariant from
    /// architecture §2.1.
    pub async fn update_pid(&self, id: &str, pid: Option<i64>) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("UPDATE sessions SET pid = ?2, updated_at = datetime('now') WHERE id = ?1")
                .bind(id)
                .bind(pid)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// All session rows whose state is in the "live" set
    /// (`starting`, `running`, `waiting`). Used by the orphan reaper on
    /// daemon startup (architecture §6.3).
    pub async fn list_active(&self) -> Result<Vec<Session>> {
        sqlx::query_as::<_, Session>(
            r#"
            SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                   worktree_path, worktree_branch, base_branch, spawn_args, origin,
                   transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                   post_create_hook_status, external_path
            FROM sessions
            WHERE state IN ('starting', 'running', 'waiting')
            "#,
        )
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    /// All archived sessions whose `archived_at` is older than the given
    /// cutoff. Drives the TTL sweep in
    /// `la_core::worktree::WorktreeManager::sweep_expired` — partial
    /// index `idx_sessions_archived_at` keeps this O(matches).
    pub async fn list_archived_older_than(&self, cutoff_rfc3339: &str) -> Result<Vec<Session>> {
        sqlx::query_as::<_, Session>(
            r#"
            SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                   worktree_path, worktree_branch, base_branch, spawn_args, origin,
                   transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                   post_create_hook_status, external_path
            FROM sessions
            WHERE archived_at IS NOT NULL AND archived_at < ?1
            "#,
        )
        .bind(cutoff_rfc3339)
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    /// Archived rows that still own a worktree directory and whose
    /// `archived_at` is older than `ttl_seconds` ago, computed inside
    /// SQLite so callers don't need a wall-clock library. Used by
    /// `la_core::worktree::WorktreeManager::sweep_expired` — narrower
    /// than [`list_archived_older_than`] so we don't reconstruct
    /// handles for rows whose worktree is already gone.
    pub async fn list_archived_with_worktree_older_than_seconds(
        &self,
        ttl_seconds: i64,
    ) -> Result<Vec<Session>> {
        // SQLite `datetime('now', '-N seconds')` returns the same
        // `YYYY-MM-DD HH:MM:SS` format that archive() writes, so a
        // plain lexicographic `<` comparison is correct.
        let modifier = format!("-{ttl_seconds} seconds");
        sqlx::query_as::<_, Session>(
            r#"
            SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                   worktree_path, worktree_branch, base_branch, spawn_args, origin,
                   transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                   post_create_hook_status, external_path
            FROM sessions
            WHERE archived_at IS NOT NULL
              AND worktree_path IS NOT NULL
              AND archived_at < datetime('now', ?1)
            "#,
        )
        .bind(modifier)
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    /// All non-archived sessions belonging to `project_id` that still
    /// own a worktree path. Helps the diff panel and prune logic find
    /// every live worktree under a given project without scanning all
    /// sessions.
    pub async fn list_with_worktree_by_project(&self, project_id: &str) -> Result<Vec<Session>> {
        sqlx::query_as::<_, Session>(
            r#"
            SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                   worktree_path, worktree_branch, base_branch, spawn_args, origin,
                   transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                   post_create_hook_status, external_path
            FROM sessions
            WHERE project_id = ?1
              AND worktree_path IS NOT NULL
              AND archived_at IS NULL
            "#,
        )
        .bind(project_id)
        .fetch_all(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }

    /// Set `worktree_path` to NULL after cleanup. When
    /// `keep_branch = false`, also nulls `worktree_branch`. Per WEK-8
    /// §2.4 row 2, the branch column survives the archive whenever the
    /// branch itself was preserved on disk — that's how the TUI can
    /// later offer "checkout this archived session's work".
    /// `base_branch` is always left intact for postmortem inspection;
    /// callers that need to wipe everything can `delete` the row.
    pub async fn clear_worktree(&self, id: &str, keep_branch: bool) -> Result<bool> {
        let sql = if keep_branch {
            "UPDATE sessions SET worktree_path = NULL, updated_at = datetime('now') WHERE id = ?1"
        } else {
            "UPDATE sessions SET worktree_path = NULL, worktree_branch = NULL, updated_at = datetime('now') WHERE id = ?1"
        };
        let result = retry_busy(|| async {
            sqlx::query(sql)
                .bind(id)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update the `post_create_hook_status` column for a session. Only
    /// the four CHECK-constrained values are accepted by SQLite; callers
    /// should pass one of `'ok' | 'failed' | 'skipped' | 'timeout'`.
    pub async fn set_post_create_hook_status(&self, id: &str, status: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query(
                "UPDATE sessions SET post_create_hook_status = ?2, updated_at = datetime('now') WHERE id = ?1",
            )
            .bind(id)
            .bind(status)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn archive(&self, id: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query(
                "UPDATE sessions SET state = 'archived', archived_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
            )
            .bind(id)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("DELETE FROM sessions WHERE id = ?1")
                .bind(id)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Look up a session by its `(backend_id, external_id)` pair so the
    /// `sessions.import` path can stay idempotent — re-importing the
    /// same backend session returns the existing row's id instead of
    /// inserting a duplicate. Returns `None` when no row matches.
    pub async fn find_by_backend_external_id(
        &self,
        backend_id: &str,
        external_id: &str,
    ) -> Result<Option<Session>> {
        sqlx::query_as::<_, Session>(
            r#"
            SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
                   worktree_path, worktree_branch, base_branch, spawn_args, origin,
                   transcript_path, transcript_bytes, created_at, updated_at, archived_at,
                   post_create_hook_status, external_path
            FROM sessions
            WHERE backend_id = ?1 AND external_id = ?2
            LIMIT 1
            "#,
        )
        .bind(backend_id)
        .bind(external_id)
        .fetch_optional(self.storage.reader_pool())
        .await
        .map_err(Into::into)
    }
}

pub struct ChunksRepo<'a> {
    storage: &'a Storage,
}

impl<'a> ChunksRepo<'a> {
    pub(crate) fn new(storage: &'a Storage) -> Self {
        Self { storage }
    }

    pub async fn append(
        &self,
        session_id: &str,
        kind: ChunkKind,
        data: impl AsRef<[u8]>,
    ) -> Result<AppendOutcome> {
        let data = data.as_ref();
        let mut tx = self.storage.writer_pool().begin().await?;
        let session = session_by_id(&mut tx, session_id)
            .await?
            .ok_or_else(|| StorageError::MissingSession(session_id.to_string()))?;
        let new_bytes = session.transcript_bytes + data.len() as i64;

        if let Some(path) = session.transcript_path {
            let path_buf = PathBuf::from(&path);
            let seq = read_spill_lines(path_buf.clone())
                .await?
                .into_iter()
                .map(|c| c.seq + 1)
                .max()
                .unwrap_or(1);
            let ts = sqlite_now_tx(&mut tx).await?;
            append_spill_line(path_buf, session_id, seq, ts, kind, data).await?;
            update_transcript_bytes(&mut tx, session_id, new_bytes).await?;
            tx.commit().await?;
            return Ok(AppendOutcome::SpilledToFile { seq, path });
        }

        if new_bytes > self.storage.transcript_spill_bytes() {
            let path = self.spill_path(session_id);
            let mut chunks = sqlx::query_as::<_, SessionChunk>(
                r#"
                SELECT session_id, seq, ts, kind, data
                FROM session_chunks
                WHERE session_id = ?1
                ORDER BY seq
                "#,
            )
            .bind(session_id)
            .fetch_all(&mut *tx)
            .await?;
            let seq = chunks.iter().map(|c| c.seq + 1).max().unwrap_or(1);
            let ts = sqlite_now_tx(&mut tx).await?;
            chunks.push(SessionChunk {
                session_id: session_id.to_string(),
                seq,
                ts,
                kind: kind.as_str().to_string(),
                data: data.to_vec(),
            });
            write_spill_file(&path, chunks).await?;
            let path_str = path.to_string_lossy().into_owned();
            sqlx::query("DELETE FROM session_chunks WHERE session_id = ?1")
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE sessions SET transcript_path = ?2, transcript_bytes = ?3, updated_at = datetime('now') WHERE id = ?1",
            )
            .bind(session_id)
            .bind(&path_str)
            .bind(new_bytes)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(AppendOutcome::SpilledToFile {
                seq,
                path: path_str,
            });
        }

        let seq: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO session_chunks(session_id, seq, kind, data)
            SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3
            FROM session_chunks
            WHERE session_id = ?1
            RETURNING seq
            "#,
        )
        .bind(session_id)
        .bind(kind.as_str())
        .bind(data)
        .fetch_one(&mut *tx)
        .await?;
        update_transcript_bytes(&mut tx, session_id, new_bytes).await?;
        tx.commit().await?;
        Ok(AppendOutcome::StoredInDb { seq })
    }

    pub async fn list(&self, session_id: &str) -> Result<Vec<SessionChunk>> {
        let mut chunks = Vec::new();
        if let Some(session) = self.storage.sessions().get(session_id).await? {
            if let Some(path) = session.transcript_path {
                chunks.extend(read_spill_lines(PathBuf::from(path)).await?);
            }
        }

        let mut db_chunks = sqlx::query_as::<_, SessionChunk>(
            r#"
            SELECT session_id, seq, ts, kind, data
            FROM session_chunks
            WHERE session_id = ?1
            ORDER BY seq
            "#,
        )
        .bind(session_id)
        .fetch_all(self.storage.reader_pool())
        .await?;
        chunks.append(&mut db_chunks);
        chunks.sort_by_key(|c| c.seq);
        Ok(chunks)
    }

    fn spill_path(&self, session_id: &str) -> PathBuf {
        self.storage
            .data_dir()
            .join("sessions")
            .join(format!("{session_id}.log"))
    }
}

pub struct SettingsRepo<'a> {
    storage: &'a Storage,
}

impl<'a> SettingsRepo<'a> {
    pub(crate) fn new(storage: &'a Storage) -> Self {
        Self { storage }
    }

    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO settings(key, value) VALUES (?1, ?2)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value
                "#,
            )
            .bind(key)
            .bind(value)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?1")
            .bind(key)
            .fetch_optional(self.storage.reader_pool())
            .await
            .map_err(Into::into)
    }

    pub async fn delete(&self, key: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query("DELETE FROM settings WHERE key = ?1")
                .bind(key)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn schema_meta(&self, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT value FROM schema_meta WHERE key = ?1")
            .bind(key)
            .fetch_optional(self.storage.reader_pool())
            .await
            .map_err(Into::into)
    }
}

async fn session_by_id(tx: &mut Transaction<'_, Sqlite>, id: &str) -> Result<Option<Session>> {
    sqlx::query_as::<_, Session>(
        r#"
        SELECT id, project_id, backend_id, external_id, title, state, exit_code, pid,
               worktree_path, worktree_branch, base_branch, spawn_args, origin,
               transcript_path, transcript_bytes, created_at, updated_at, archived_at,
               post_create_hook_status, external_path
        FROM sessions
        WHERE id = ?1
        "#,
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(Into::into)
}

const CRON_SELECT_BY_ID: &str = r#"
SELECT id, name, enabled, project_id, backend_id, spawn_args, prompt,
       cron_expr, tz, catchup_mode, max_concurrent_runs, max_runs_per_day,
       max_runtime_s, cost_budget_usd_per_day, failure_backoff,
       pause_on_consecutive_failures, consecutive_failures, last_fired_at,
       next_fire_at, created_at, updated_at
FROM crons
WHERE id = ?1
"#;

const RUN_SELECT_BY_ID: &str = r#"
SELECT id, cron_id, session_id, scheduled_at, started_at, finished_at,
       status, exit_code, coalesced_count, cost_usd_est, error_kind,
       error_detail, tail_log
FROM runs
WHERE id = ?1
"#;

fn archive_month_key(scheduled_at: &str) -> String {
    let digits: String = scheduled_at
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(6)
        .collect();
    if digits.len() == 6 {
        digits
    } else {
        "unknown".to_string()
    }
}

async fn append_runs_archive(path: &Path, rows: &[RunRecord]) -> Result<()> {
    let path = path.to_path_buf();
    let lines = rows
        .iter()
        .cloned()
        .map(|row| serde_json::to_string(&RunArchiveLine::from(row)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let mut encoder = zstd::stream::write::Encoder::new(file, 0)?;
        for line in lines {
            use std::io::Write;
            encoder.write_all(line.as_bytes())?;
            encoder.write_all(b"\n")?;
        }
        encoder.finish()?;
        Ok(())
    })
    .await??;
    Ok(())
}

async fn update_transcript_bytes(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &str,
    bytes: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE sessions SET transcript_bytes = ?2, updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(session_id)
    .bind(bytes)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn append_spill_line(
    path: PathBuf,
    session_id: &str,
    seq: i64,
    ts: String,
    kind: ChunkKind,
    data: &[u8],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let line = SpillLine {
        session_id: session_id.to_string(),
        seq,
        ts,
        kind: kind.as_str().to_string(),
        data_base64: STANDARD.encode(data),
    };
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(serde_json::to_string(&line)?.as_bytes())
        .await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

async fn write_spill_file(path: &PathBuf, chunks: Vec<SessionChunk>) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    for chunk in chunks {
        let line = SpillLine {
            session_id: chunk.session_id,
            seq: chunk.seq,
            ts: chunk.ts,
            kind: chunk.kind,
            data_base64: STANDARD.encode(chunk.data),
        };
        file.write_all(serde_json::to_string(&line)?.as_bytes())
            .await?;
        file.write_all(b"\n").await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

async fn retry_busy<F, Fut>(mut op: F) -> Result<SqliteQueryResult>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<SqliteQueryResult, SqlxError>>,
{
    const MAX_ATTEMPTS: usize = 8;

    for attempt in 1..=MAX_ATTEMPTS {
        match op().await {
            Ok(result) => return Ok(result),
            Err(err) if is_busy(&err) && attempt < MAX_ATTEMPTS => {
                let backoff_ms = (50_u64 * (1_u64 << (attempt - 1))).min(500);
                let jitter_ms = (attempt as u64 * 17) % 41;
                tokio::time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            }
            Err(err) if is_busy(&err) => return Err(StorageError::Busy { attempts: attempt }),
            Err(err) => return Err(err.into()),
        }
    }

    Err(StorageError::Busy {
        attempts: MAX_ATTEMPTS,
    })
}

fn is_busy(err: &SqlxError) -> bool {
    match err {
        SqlxError::Database(db) => {
            db.code().as_deref() == Some("5") || db.message().contains("locked")
        }
        _ => false,
    }
}

async fn sqlite_now_tx(tx: &mut Transaction<'_, Sqlite>) -> Result<String> {
    sqlx::query_scalar("SELECT datetime('now')")
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
}

async fn read_spill_lines(path: PathBuf) -> Result<Vec<SessionChunk>> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut chunks = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let spill: SpillLine = serde_json::from_str(line)?;
        chunks.push(SessionChunk {
            session_id: spill.session_id,
            seq: spill.seq,
            ts: spill.ts,
            kind: spill.kind,
            data: STANDARD.decode(spill.data_base64)?,
        });
    }
    Ok(chunks)
}
