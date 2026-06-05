use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::sqlite::SqliteQueryResult;
use sqlx::{Error as SqlxError, Sqlite, Transaction};
use tokio::io::AsyncWriteExt;

use crate::models::{
    AppendOutcome, Backend, BackendUpsert, ChunkKind, Cron, CronUpsert, NewProject, NewRejectedRun,
    NewRun, NewSession, Project, RunArchiveLine, RunFinish, RunRecord, RunsArchiveOutcome,
    RunsListFilter, Session, SessionChunk, SpillLine,
};
use crate::{Result, Storage, StorageError};

/// Map a raw `runs.status` value (`completed` / `failed` / `timed_out` /
/// `cancelled` / `budget_exceeded` / ...) onto the four canonical label
/// values pinned by the A9 metric naming table for
/// `lad_cron_runs_total{status}` (`ok` / `error` / `timeout` /
/// `budget_rejected`).
///
/// Architecture §9.3 (M4.5 / WEK-75): the metric is a contract surface
/// — dashboard panels and Prometheus alert rules slice on these four
/// values. The raw schema enum is intentionally wider (it carries the
/// audit detail needed for `lad runs get`), so the mapping happens at
/// the emit boundary instead of leaking schema vocabulary outward.
///
/// Anything we don't recognise falls back to `error` rather than the
/// silently-inserted unknown label that would otherwise stretch the
/// metric's cardinality forever.
pub(crate) fn cron_status_label(raw: &str) -> &'static str {
    match raw {
        "completed" => "ok",
        "timed_out" => "timeout",
        // Budget rejection carries either the legacy `budget_exceeded`
        // or any future `*_rejected` flavour; map both to the
        // contract's `budget_rejected`.
        "budget_exceeded" => "budget_rejected",
        s if s.ends_with("_rejected") => "budget_rejected",
        // `failed` / `cancelled` and anything else terminal-but-bad
        // collapses to the `error` bucket per the A9 table; the
        // detailed reason still rides on `runs.error_kind`.
        _ => "error",
    }
}

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
                  AND (
                    last_fired_at IS NULL
                    OR last_fired_at < ?2
                    OR (last_fired_at = ?2 AND next_fire_at IS ?3)
                  )
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

    /// Atomically increment `consecutive_failures` by 1 and return the
    /// post-increment counter value. WEK-33 / M3.2 uses this on every
    /// terminal-failure run completion so an immediate `pause_for_failures`
    /// can compare against the threshold without re-reading the row.
    ///
    /// Returns `Ok(None)` if the cron does not exist.
    pub async fn bump_consecutive_failures(&self, id: &str) -> Result<Option<i64>> {
        retry_busy(|| async {
            sqlx::query(
                r#"
                UPDATE crons
                SET consecutive_failures = consecutive_failures + 1,
                    updated_at = datetime('now')
                WHERE id = ?1
                "#,
            )
            .bind(id)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT consecutive_failures FROM crons WHERE id = ?1")
                .bind(id)
                .fetch_optional(self.storage.reader_pool())
                .await?;
        Ok(row.map(|(v,)| v))
    }

    /// Reset `consecutive_failures` to 0 (called on a `completed` run).
    /// Returns `Ok(true)` when a row was actually changed, `Ok(false)` when
    /// the counter was already zero or the cron does not exist.
    pub async fn reset_consecutive_failures(&self, id: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query(
                r#"
                UPDATE crons
                SET consecutive_failures = 0, updated_at = datetime('now')
                WHERE id = ?1 AND consecutive_failures != 0
                "#,
            )
            .bind(id)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Atomically flip `enabled = 0` iff the consecutive-failure threshold
    /// has been met. Returns `Ok(true)` when the cron was paused by this
    /// call; `Ok(false)` when the threshold was not yet met OR the cron was
    /// already disabled. This is the auto-disable hook for §5.4
    /// "pause_on_consecutive_failures".
    ///
    /// We re-evaluate the threshold inside the same UPDATE so a concurrent
    /// `reset_consecutive_failures` (a competing success path) cannot lose
    /// to a stale "should pause" decision computed before the update.
    pub async fn pause_for_failures(&self, id: &str) -> Result<bool> {
        let result = retry_busy(|| async {
            sqlx::query(
                r#"
                UPDATE crons
                SET enabled = 0, updated_at = datetime('now')
                WHERE id = ?1
                  AND enabled = 1
                  AND pause_on_consecutive_failures > 0
                  AND consecutive_failures >= pause_on_consecutive_failures
                "#,
            )
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
        let started = std::time::Instant::now();
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
        metrics::histogram!("lad_storage_write_latency_seconds", "op" => "runs.create")
            .record(started.elapsed().as_secs_f64());
        self.get(&run.id)
            .await?
            .ok_or(StorageError::MissingRun(run.id))
    }

    /// Insert a "rejected before spawn" audit row in a single statement —
    /// `started_at IS NULL`, `finished_at = scheduled_at` (the moment the
    /// gate refused), `exit_code IS NULL`, with the rejection reason in
    /// `error_kind` / `error_detail`. WEK-33 / M3.2 admission control uses
    /// this for every quota refusal that cannot ride through the normal
    /// `runs.finish` path (because no run was ever started).
    ///
    /// `status` must be one of the schema enum strings ("budget_exceeded"
    /// or "cancelled" today); SQLite's CHECK on `runs.status` enforces it.
    pub async fn create_rejected(&self, rejected: NewRejectedRun<'_>) -> Result<RunRecord> {
        let started = std::time::Instant::now();
        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO runs(
                    id, cron_id, session_id, scheduled_at, started_at,
                    finished_at, status, exit_code, coalesced_count,
                    cost_usd_est, error_kind, error_detail
                )
                VALUES (?1, ?2, NULL, ?3, NULL, ?3, ?4, NULL, ?5, NULL, ?6, ?7)
                "#,
            )
            .bind(rejected.id)
            .bind(rejected.cron_id)
            .bind(rejected.scheduled_at)
            .bind(rejected.status)
            .bind(rejected.coalesced_count.max(1))
            .bind(rejected.error_kind)
            .bind(rejected.error_detail)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        metrics::histogram!("lad_storage_write_latency_seconds", "op" => "runs.create_rejected")
            .record(started.elapsed().as_secs_f64());
        metrics::counter!(
            "lad_cron_runs_total",
            "status" => cron_status_label(rejected.status),
        )
        .increment(1);
        self.get(rejected.id)
            .await?
            .ok_or_else(|| StorageError::MissingRun(rejected.id.to_string()))
    }

    /// Count rows for `cron_id` that are still in-flight (status in
    /// `pending|spawning|running` and `finished_at IS NULL`). Used by
    /// `max_concurrent_runs` enforcement.
    pub async fn count_running_for_cron(&self, cron_id: &str) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM runs
            WHERE cron_id = ?1
              AND finished_at IS NULL
              AND status IN ('pending','spawning','running')
            "#,
        )
        .bind(cron_id)
        .fetch_one(self.storage.reader_pool())
        .await?;
        Ok(count)
    }

    /// Global in-flight count across all crons. Used by
    /// `global_max_concurrent_runs`.
    pub async fn count_running_global(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM runs
            WHERE finished_at IS NULL
              AND status IN ('pending','spawning','running')
            "#,
        )
        .fetch_one(self.storage.reader_pool())
        .await?;
        Ok(count)
    }

    /// Count rows for `cron_id` whose `scheduled_at >= since` (typically
    /// `now - 24h`).
    ///
    /// **Excludes quota-refusal audit rows** (`error_kind LIKE 'quota_%'`)
    /// so the `max_runs_per_day` rail in [`la_scheduler::quota`] only
    /// counts admitted attempts. If we included quota audit rows, a
    /// high-frequency cron that hit the cap once would keep writing
    /// refusal rows on every subsequent tick, and each new row would
    /// extend the window count above the cap — the cron would be
    /// permanently denied even after the original admitted runs roll out
    /// of the 24h window. Audit rows still appear in
    /// [`RunsRepo::list`] for the TUI.
    pub async fn count_since_for_cron(&self, cron_id: &str, since: &str) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM runs
            WHERE cron_id = ?1
              AND scheduled_at >= ?2
              AND (error_kind IS NULL OR error_kind NOT LIKE 'quota\_%' ESCAPE '\')
            "#,
        )
        .bind(cron_id)
        .bind(since)
        .fetch_one(self.storage.reader_pool())
        .await?;
        Ok(count)
    }

    /// Sum `cost_usd_est` for `cron_id` since the given lexical timestamp.
    /// Rows with NULL cost are treated as 0 (the field is only populated
    /// when the adapter reports usage; absent costs don't consume budget).
    /// Excludes quota-refusal audit rows for the same reason as
    /// [`Self::count_since_for_cron`] — a quota refusal must not consume
    /// the budget it was refused under.
    pub async fn sum_cost_since_for_cron(&self, cron_id: &str, since: &str) -> Result<f64> {
        let (sum,): (Option<f64>,) = sqlx::query_as(
            r#"
            SELECT COALESCE(SUM(cost_usd_est), 0.0) FROM runs
            WHERE cron_id = ?1
              AND scheduled_at >= ?2
              AND (error_kind IS NULL OR error_kind NOT LIKE 'quota\_%' ESCAPE '\')
            "#,
        )
        .bind(cron_id)
        .bind(since)
        .fetch_one(self.storage.reader_pool())
        .await?;
        Ok(sum.unwrap_or(0.0))
    }

    /// Most recent `finished_at` for a terminal-failure run of `cron_id`
    /// (`status IN ('failed','timed_out')`). Feeds the
    /// `failure_backoff` rail in [`la_scheduler::quota`].
    /// Returns the raw SQLite lexical timestamp; callers parse it into
    /// `chrono::DateTime<Utc>`.
    pub async fn last_terminal_failure_at_for_cron(&self, cron_id: &str) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            r#"
            SELECT MAX(finished_at) FROM runs
            WHERE cron_id = ?1
              AND finished_at IS NOT NULL
              AND status IN ('failed', 'timed_out')
            "#,
        )
        .bind(cron_id)
        .fetch_optional(self.storage.reader_pool())
        .await?;
        Ok(row.and_then(|(v,)| v))
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
        let started = std::time::Instant::now();
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
        metrics::histogram!("lad_storage_write_latency_seconds", "op" => "runs.attach_session")
            .record(started.elapsed().as_secs_f64());
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_status(&self, id: &str, status: &str) -> Result<bool> {
        let started = std::time::Instant::now();
        let result = retry_busy(|| async {
            sqlx::query("UPDATE runs SET status = ?2 WHERE id = ?1 AND finished_at IS NULL")
                .bind(id)
                .bind(status)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        metrics::histogram!("lad_storage_write_latency_seconds", "op" => "runs.update_status")
            .record(started.elapsed().as_secs_f64());
        Ok(result.rows_affected() > 0)
    }

    pub async fn finish(&self, id: &str, finish: RunFinish) -> Result<bool> {
        let started = std::time::Instant::now();
        let tail_log = truncate_tail_log(finish.tail_log.clone());
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
                  AND finished_at IS NULL
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
        metrics::histogram!("lad_storage_write_latency_seconds", "op" => "runs.finish")
            .record(started.elapsed().as_secs_f64());
        if result.rows_affected() > 0 {
            metrics::counter!(
                "lad_cron_runs_total",
                "status" => cron_status_label(&finish.status),
            )
            .increment(1);
            return Ok(true);
        }

        let Some(existing) = self.get(id).await? else {
            return Ok(false);
        };
        Ok(
            existing.finished_at.as_deref() == Some(finish.finished_at.as_str())
                && existing.status == finish.status
                && existing.exit_code == finish.exit_code
                && existing.cost_usd_est == finish.cost_usd_est
                && existing.error_kind == finish.error_kind
                && existing.error_detail == finish.error_detail
                && existing.tail_log == tail_log,
        )
    }

    /// Archive runs whose `finished_at` falls strictly before
    /// `now - retention_days` (M3 Rev2 §3.3). Only rows in a terminal
    /// status are considered (`completed`, `failed`, `timed_out`,
    /// `cancelled`, `budget_exceeded`); `pending`,
    /// `spawning`, and `running` rows are left in place even if their
    /// `scheduled_at` is ancient — those represent live or stranded work
    /// that the operator should resolve manually, not data to discard.
    ///
    /// Internally batched (Rev2 §S5): each batch reads up to
    /// [`ARCHIVE_BATCH_SIZE`] rows, appends them to the per-month
    /// `runs/archive/<yyyymm>.jsonl.zst` file with a full
    /// file + parent-directory `fsync`, then opens a single SQLite
    /// transaction that `DELETE`s exactly that batch. If the commit
    /// fails, the batch is dropped and the next pass re-reads the same
    /// window — the JSONL file may contain duplicate entries for that
    /// batch, which recovery handles by keeping the last write for any
    /// `id`. The loop continues until a read returns fewer than
    /// `ARCHIVE_BATCH_SIZE` rows.
    pub async fn archive_older_than_days(&self, retention_days: i64) -> Result<RunsArchiveOutcome> {
        let modifier = format!("-{} days", retention_days.max(1));
        let archive_dir = self.storage.data_dir().join("runs").join("archive");
        tokio::fs::create_dir_all(&archive_dir).await?;

        let mut archived_rows: u64 = 0;
        let mut archive_files_touched: HashSet<String> = HashSet::new();

        loop {
            // Read one batch outside the writer tx so a slow archive
            // pass doesn't hold a writer lock across fsync.
            let rows = sqlx::query_as::<_, RunRecord>(
                r#"
                SELECT id, cron_id, session_id, scheduled_at, started_at, finished_at,
                       status, exit_code, coalesced_count, cost_usd_est, error_kind,
                       error_detail, tail_log
                FROM runs
                WHERE finished_at IS NOT NULL
                  AND finished_at < datetime('now', ?1)
                  AND status IN (
                        'completed',
                        'failed',
                        'timed_out',
                        'cancelled',
                        'budget_exceeded'
                      )
                ORDER BY finished_at, id
                LIMIT ?2
                "#,
            )
            .bind(&modifier)
            .bind(ARCHIVE_BATCH_SIZE)
            .fetch_all(self.storage.reader_pool())
            .await?;

            if rows.is_empty() {
                break;
            }
            let batch_size = rows.len();

            // Group this batch by month, append → fsync per file.
            let mut by_month: BTreeMap<String, Vec<RunRecord>> = BTreeMap::new();
            for row in rows {
                by_month
                    .entry(archive_month_key(&row.scheduled_at))
                    .or_default()
                    .push(row);
            }

            let mut batch_ids: Vec<String> = Vec::new();
            for (month, month_rows) in by_month.iter() {
                let path = archive_dir.join(format!("{month}.jsonl.zst"));
                append_runs_archive(&path, month_rows).await?;
                archive_files_touched.insert(month.clone());
                batch_ids.extend(month_rows.iter().map(|row| row.id.clone()));
            }

            // Atomic DELETE: one tx, one DELETE ... IN (?, ?, ...) so
            // either every row in this batch is removed or none is.
            let mut tx = self.storage.writer_pool().begin().await?;
            sqlx::query("UPDATE schema_meta SET value = value WHERE key = 'schema_version'")
                .execute(&mut *tx)
                .await?;
            let placeholders = vec!["?"; batch_ids.len()].join(",");
            let delete_sql = format!("DELETE FROM runs WHERE id IN ({placeholders})");
            let mut query = sqlx::query(&delete_sql);
            for id in &batch_ids {
                query = query.bind(id);
            }
            query.execute(&mut *tx).await?;
            tx.commit().await?;

            archived_rows += batch_size as u64;
            if batch_size < ARCHIVE_BATCH_SIZE as usize {
                break;
            }
        }

        Ok(RunsArchiveOutcome {
            archived_rows,
            archive_files: archive_files_touched.len(),
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
        let started = std::time::Instant::now();
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
            metrics::histogram!("lad_storage_write_latency_seconds", "op" => "chunks.append")
                .record(started.elapsed().as_secs_f64());
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
            metrics::histogram!("lad_storage_write_latency_seconds", "op" => "chunks.append")
                .record(started.elapsed().as_secs_f64());
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
        metrics::histogram!("lad_storage_write_latency_seconds", "op" => "chunks.append")
            .record(started.elapsed().as_secs_f64());
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

/// Maximum number of `runs` rows the archive loop reads and deletes per
/// atomic batch (Rev2 §S5). Picked to keep the write transaction short
/// enough that backup / read traffic doesn't stall, while still
/// amortising the per-batch fsync cost.
const ARCHIVE_BATCH_SIZE: i64 = 500;

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

fn truncate_tail_log(tail_log: Option<Vec<u8>>) -> Option<Vec<u8>> {
    tail_log.map(|mut bytes| {
        if bytes.len() > 64 * 1024 {
            bytes = bytes.split_off(bytes.len() - 64 * 1024);
        }
        bytes
    })
}

/// Append a batch of runs to the per-month `.jsonl.zst` archive file and
/// `fsync` both the file and its parent directory before returning
/// (Rev2 §S5). After this call returns Ok, the bytes are durable enough
/// that a subsequent power-cut will not lose them — which is the
/// pre-condition for the caller's atomic `DELETE` of the same rows from
/// SQLite. Hook the sync path via [`archive_sync_hook`] from tests so a
/// fault-injecting fixture can observe each fsync separately.
async fn append_runs_archive(path: &Path, rows: &[RunRecord]) -> Result<()> {
    let path = path.to_path_buf();
    let parent = path
        .parent()
        .ok_or_else(|| {
            StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "archive path has no parent",
            ))
        })?
        .to_path_buf();
    let lines = rows
        .iter()
        .cloned()
        .map(|row| serde_json::to_string(&RunArchiveLine::from(row)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut encoder = zstd::stream::write::Encoder::new(file, 0)?;
        for line in lines {
            use std::io::Write;
            encoder.write_all(line.as_bytes())?;
            encoder.write_all(b"\n")?;
        }
        let file = encoder.finish()?;
        file.sync_all()?;
        archive_sync_hook(ArchiveSyncEvent::File(&path))?;
        // Parent directory fsync — required so the new file entry is
        // durable on crash-restart, not just the file's data. POSIX
        // permits opening a directory read-only for fsync; on Windows
        // this is a no-op (the `std::fs::File::sync_all` semantics
        // there already cover the directory entry).
        #[cfg(unix)]
        {
            let dir = std::fs::File::open(&parent)?;
            dir.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            let _ = &parent;
        }
        archive_sync_hook(ArchiveSyncEvent::ParentDir(&parent))?;
        Ok(())
    })
    .await??;
    Ok(())
}

/// One archive batch attempts to fsync the data file, then the parent
/// directory. Tests pin a hook into [`archive_sync_hook`] to count
/// these events (proving both happen) and to inject I/O failures
/// between them.
#[derive(Debug)]
pub enum ArchiveSyncEvent<'a> {
    File(&'a Path),
    ParentDir(&'a Path),
}

#[cfg(test)]
pub(crate) type ArchiveSyncHook =
    Box<dyn Fn(&ArchiveSyncEvent<'_>) -> std::io::Result<()> + Send + Sync>;

#[cfg(test)]
static ARCHIVE_SYNC_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<ArchiveSyncHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn set_archive_sync_hook(hook: Option<ArchiveSyncHook>) {
    let cell = ARCHIVE_SYNC_HOOK.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().unwrap() = hook;
}

#[inline]
fn archive_sync_hook(event: ArchiveSyncEvent<'_>) -> std::io::Result<()> {
    #[cfg(test)]
    {
        if let Some(cell) = ARCHIVE_SYNC_HOOK.get() {
            if let Some(hook) = cell.lock().unwrap().as_ref() {
                return hook(&event);
            }
        }
    }
    #[cfg(not(test))]
    {
        let _ = event;
    }
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

#[cfg(test)]
mod archive_unit_tests {
    //! WEK-60: tests that pin down behaviours which are awkward to
    //! observe through the integration suite — fsync ordering, the
    //! status-filter (running rows must not be archived), and the
    //! commit-failure rollback path.
    //!
    //! Hook tests serialize on [`HOOK_LOCK`] because
    //! `ARCHIVE_SYNC_HOOK` is a process-wide static; running them
    //! concurrently would let one test see another's injected hook.

    use super::*;
    use crate::storage::Storage;
    use crate::StorageConfig;
    use crate::{NewProject, NewRun, RunFinish};
    use std::sync::{Arc, Mutex as StdMutex};
    use tempfile::TempDir;

    static HOOK_LOCK: StdMutex<()> = StdMutex::new(());

    async fn open_storage_with_project_and_cron() -> (TempDir, Storage) {
        let dir = TempDir::new().expect("tempdir");
        let storage = Storage::open(StorageConfig::for_test(dir.path()))
            .await
            .expect("open");
        storage
            .backends()
            .upsert(crate::BackendUpsert {
                id: "claude",
                display_name: "Claude Code",
                version: None,
                available: true,
            })
            .await
            .unwrap();
        storage
            .projects()
            .create(NewProject {
                id: "proj".into(),
                root_path: "/tmp/wek60/proj".into(),
                display_name: "proj".into(),
                vcs: Some("git".into()),
            })
            .await
            .unwrap();
        storage
            .crons()
            .upsert(CronUpsert {
                id: "cron".into(),
                name: "n".into(),
                enabled: true,
                project_id: "proj".into(),
                backend_id: "claude".into(),
                spawn_args: serde_json::json!({}),
                prompt: "p".into(),
                cron_expr: "17 3 * * *".into(),
                tz: "UTC".into(),
                catchup_mode: "coalesce".into(),
                max_concurrent_runs: 1,
                max_runs_per_day: 24,
                max_runtime_s: 1800,
                cost_budget_usd_per_day: None,
                failure_backoff: "expo(1m,2,1h)".into(),
                pause_on_consecutive_failures: 5,
                consecutive_failures: 0,
                last_fired_at: None,
                next_fire_at: None,
            })
            .await
            .unwrap();
        (dir, storage)
    }

    async fn insert_finished_run(storage: &Storage, id: &str, finished_at: &str) {
        storage
            .runs()
            .create(NewRun {
                id: id.into(),
                cron_id: Some("cron".into()),
                session_id: None,
                scheduled_at: "2000-01-01 03:17:00".into(),
                started_at: Some("2000-01-01 03:17:01".into()),
                status: "running".into(),
                coalesced_count: 1,
            })
            .await
            .unwrap();
        storage
            .runs()
            .finish(
                id,
                RunFinish {
                    finished_at: finished_at.into(),
                    status: "completed".into(),
                    exit_code: Some(0),
                    cost_usd_est: None,
                    error_kind: None,
                    error_detail: None,
                    tail_log: None,
                },
            )
            .await
            .unwrap();
    }

    /// Rev2 §S5 acceptance: every archive write must fsync the data
    /// file AND the parent directory before the SQLite DELETE commits.
    /// The hook here records what the hot path actually called.
    #[tokio::test(flavor = "current_thread")]
    async fn archive_writes_fsync_file_then_parent_dir() {
        let _guard = HOOK_LOCK.lock().unwrap();
        let (_dir, storage) = open_storage_with_project_and_cron().await;
        insert_finished_run(&storage, "old", "2000-01-01 03:18:00").await;

        let events: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let log = events.clone();
        set_archive_sync_hook(Some(Box::new(move |event| {
            let tag = match event {
                ArchiveSyncEvent::File(p) => format!("file:{}", p.display()),
                ArchiveSyncEvent::ParentDir(p) => format!("dir:{}", p.display()),
            };
            log.lock().unwrap().push(tag);
            Ok(())
        })));

        let outcome = storage.runs().archive_older_than_days(90).await.unwrap();
        set_archive_sync_hook(None);
        assert_eq!(outcome.archived_rows, 1);

        let events = events.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "expected file fsync + dir fsync");
        assert!(events[0].starts_with("file:"), "got {events:?}");
        assert!(events[1].starts_with("dir:"), "got {events:?}");
        assert!(events[0].contains("200001.jsonl.zst"));
        assert!(events[1].ends_with("archive"));
        // Row was deleted only because the fsync hook returned Ok.
        assert!(storage.runs().get("old").await.unwrap().is_none());
    }

    /// Rev2 §3.3: status='running' rows older than the retention window
    /// must NOT be archived — those represent live or stranded work,
    /// and silently deleting them would lose evidence that an operator
    /// needs.
    #[tokio::test(flavor = "current_thread")]
    async fn archive_skips_running_rows_even_when_old() {
        let (_dir, storage) = open_storage_with_project_and_cron().await;

        // A still-running run created 1000 days ago. Its `scheduled_at`
        // is ancient, but it has no finished_at and its status is
        // 'running' — both criteria must protect it from archive.
        storage
            .runs()
            .create(NewRun {
                id: "running-old".into(),
                cron_id: Some("cron".into()),
                session_id: None,
                scheduled_at: "2000-01-01 03:17:00".into(),
                started_at: Some("2000-01-01 03:17:01".into()),
                status: "running".into(),
                coalesced_count: 1,
            })
            .await
            .unwrap();
        // A pending row is similarly ancient — also must not be archived.
        storage
            .runs()
            .create(NewRun {
                id: "pending-old".into(),
                cron_id: Some("cron".into()),
                session_id: None,
                scheduled_at: "2000-01-01 03:17:00".into(),
                started_at: None,
                status: "pending".into(),
                coalesced_count: 1,
            })
            .await
            .unwrap();

        let outcome = storage.runs().archive_older_than_days(90).await.unwrap();
        assert_eq!(outcome.archived_rows, 0);
        assert!(storage.runs().get("running-old").await.unwrap().is_some());
        assert!(storage.runs().get("pending-old").await.unwrap().is_some());
    }

    /// Rev2 §S5: if the fsync (or any pre-commit step) fails, the
    /// SQLite DELETE must NOT happen. The next archive pass re-reads
    /// the same row, the JSONL ends up with a duplicate entry for the
    /// row's id, and the documented recovery rule (last write wins on
    /// id) collapses them.
    #[tokio::test(flavor = "current_thread")]
    async fn commit_failure_rolls_back_batch_and_dedups_by_last_write() {
        let _guard = HOOK_LOCK.lock().unwrap();
        let (_dir, storage) = open_storage_with_project_and_cron().await;
        insert_finished_run(&storage, "rollback-old", "2000-01-01 03:18:00").await;

        // First pass: fsync hook fails -> append_runs_archive returns
        // Err, the surrounding `archive_older_than_days` propagates it
        // before the DELETE tx ever opens, so the row remains in
        // SQLite. Whatever bytes the encoder already wrote to disk
        // before the fsync error are still there (zstd encoder.finish()
        // ran in the spawn_blocking task), so the archive file may
        // exist on disk with content for this row.
        set_archive_sync_hook(Some(Box::new(|event| match event {
            ArchiveSyncEvent::File(_) => Err(std::io::Error::other("injected fsync failure")),
            ArchiveSyncEvent::ParentDir(_) => Ok(()),
        })));
        let err = storage
            .runs()
            .archive_older_than_days(90)
            .await
            .expect_err("expected fsync-induced failure");
        let _ = err;
        set_archive_sync_hook(None);
        assert!(
            storage.runs().get("rollback-old").await.unwrap().is_some(),
            "row must remain when fsync fails"
        );

        // Second pass with the hook restored: the same row gets read
        // again, appended to the file again, and deleted. The archive
        // file now contains a duplicate entry for "rollback-old" —
        // that is intentional per the recovery contract.
        let outcome = storage.runs().archive_older_than_days(90).await.unwrap();
        assert_eq!(outcome.archived_rows, 1);
        assert!(storage.runs().get("rollback-old").await.unwrap().is_none());

        // Verify the JSONL file actually has >=1 entry for this id;
        // last-write-wins dedup turns the entries into a single
        // logical record.
        let archive = storage.data_dir().join("runs/archive/200001.jsonl.zst");
        let lines = read_archive_lines_for_test(&archive);
        let matching = lines
            .iter()
            .filter(|l| l.contains("\"id\":\"rollback-old\""))
            .count();
        assert!(matching >= 1, "expected ≥1 jsonl line for rollback-old");
        let deduped: std::collections::HashMap<String, String> = lines
            .iter()
            .filter_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).ok()?;
                let id = v.get("id")?.as_str()?.to_string();
                Some((id, line.clone()))
            })
            .collect();
        assert!(deduped.contains_key("rollback-old"));
    }

    fn read_archive_lines_for_test(path: &Path) -> Vec<String> {
        let file = std::fs::File::open(path).expect("archive file");
        let decoder = zstd::stream::read::Decoder::new(file).expect("zstd decoder");
        use std::io::BufRead;
        std::io::BufReader::new(decoder)
            .lines()
            .map(|line| line.expect("line"))
            .filter(|line| !line.trim().is_empty())
            .collect()
    }
}
