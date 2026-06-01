use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::sqlite::SqliteQueryResult;
use sqlx::{Error as SqlxError, SqlitePool};
use tokio::io::AsyncWriteExt;

use crate::models::{
    AppendOutcome, Backend, BackendUpsert, ChunkKind, NewProject, NewSession, Project, Session,
    SessionChunk, SpillLine,
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
                    worktree_path, worktree_branch, base_branch, spawn_args, origin
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
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
                   transcript_path, transcript_bytes, created_at, updated_at, archived_at
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
                       transcript_path, transcript_bytes, created_at, updated_at, archived_at
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
                       transcript_path, transcript_bytes, created_at, updated_at, archived_at
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
        let seq = self.next_seq(session_id).await?;
        let session = self
            .storage
            .sessions()
            .get(session_id)
            .await?
            .ok_or_else(|| StorageError::MissingSession(session_id.to_string()))?;
        let new_bytes = session.transcript_bytes + data.len() as i64;

        if let Some(path) = session.transcript_path {
            self.append_spill_line(PathBuf::from(&path), session_id, seq, kind, data)
                .await?;
            self.bump_transcript_bytes(session_id, new_bytes).await?;
            return Ok(AppendOutcome::SpilledToFile { seq, path });
        }

        if new_bytes > self.storage.transcript_spill_bytes() {
            let path = self.spill_path(session_id);
            self.spill_existing_chunks(&path, session_id).await?;
            self.append_spill_line(path.clone(), session_id, seq, kind, data)
                .await?;
            let path_str = path.to_string_lossy().into_owned();
            self.mark_spilled(session_id, &path_str, new_bytes).await?;
            return Ok(AppendOutcome::SpilledToFile {
                seq,
                path: path_str,
            });
        }

        retry_busy(|| async {
            sqlx::query(
                r#"
                INSERT INTO session_chunks(session_id, seq, kind, data)
                VALUES (?1, ?2, ?3, ?4)
                "#,
            )
            .bind(session_id)
            .bind(seq)
            .bind(kind.as_str())
            .bind(data)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        self.bump_transcript_bytes(session_id, new_bytes).await?;
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

    async fn next_seq(&self, session_id: &str) -> Result<i64> {
        let db_next: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) + 1 FROM session_chunks WHERE session_id = ?1")
                .bind(session_id)
                .fetch_one(self.storage.reader_pool())
                .await?;

        let spill_next = if let Some(session) = self.storage.sessions().get(session_id).await? {
            if let Some(path) = session.transcript_path {
                read_spill_lines(PathBuf::from(path))
                    .await?
                    .into_iter()
                    .map(|c| c.seq + 1)
                    .max()
            } else {
                None
            }
        } else {
            None
        };

        Ok(db_next.or(spill_next).unwrap_or(1))
    }

    async fn spill_existing_chunks(&self, path: &PathBuf, session_id: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let chunks = sqlx::query_as::<_, SessionChunk>(
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

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
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

        retry_busy(|| async {
            sqlx::query("DELETE FROM session_chunks WHERE session_id = ?1")
                .bind(session_id)
                .execute(self.storage.writer_pool())
                .await
        })
        .await?;
        Ok(())
    }

    async fn append_spill_line(
        &self,
        path: PathBuf,
        session_id: &str,
        seq: i64,
        kind: ChunkKind,
        data: &[u8],
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let ts = sqlite_now(self.storage.reader_pool()).await?;
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
        Ok(())
    }

    async fn mark_spilled(&self, session_id: &str, path: &str, bytes: i64) -> Result<()> {
        retry_busy(|| async {
            sqlx::query(
                "UPDATE sessions SET transcript_path = ?2, transcript_bytes = ?3, updated_at = datetime('now') WHERE id = ?1",
            )
            .bind(session_id)
            .bind(path)
            .bind(bytes)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(())
    }

    async fn bump_transcript_bytes(&self, session_id: &str, bytes: i64) -> Result<()> {
        retry_busy(|| async {
            sqlx::query(
                "UPDATE sessions SET transcript_bytes = ?2, updated_at = datetime('now') WHERE id = ?1",
            )
            .bind(session_id)
            .bind(bytes)
            .execute(self.storage.writer_pool())
            .await
        })
        .await?;
        Ok(())
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

async fn sqlite_now(pool: &SqlitePool) -> Result<String> {
    sqlx::query_scalar("SELECT datetime('now')")
        .fetch_one(pool)
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
