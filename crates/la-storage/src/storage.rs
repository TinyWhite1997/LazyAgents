use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, SqlitePool};

use crate::{repos, Result, StorageError, CURRENT_SCHEMA_VERSION, MIGRATOR};

#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub database_path: PathBuf,
    pub data_dir: PathBuf,
    pub read_max_connections: u32,
    pub busy_timeout: Duration,
    pub transcript_spill_bytes: i64,
}

impl StorageConfig {
    pub fn new(database_path: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            database_path: database_path.into(),
            data_dir: data_dir.into(),
            read_max_connections: 4,
            busy_timeout: Duration::from_secs(5),
            transcript_spill_bytes: crate::DEFAULT_TRANSCRIPT_SPILL_BYTES,
        }
    }

    pub fn for_test(dir: &Path) -> Self {
        Self {
            database_path: dir.join("lad.sqlite"),
            data_dir: dir.to_path_buf(),
            read_max_connections: 4,
            busy_timeout: Duration::from_millis(50),
            transcript_spill_bytes: crate::DEFAULT_TRANSCRIPT_SPILL_BYTES,
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    writer: SqlitePool,
    reader: SqlitePool,
    database_path: PathBuf,
    data_dir: PathBuf,
    transcript_spill_bytes: i64,
}

impl Storage {
    pub async fn open(config: StorageConfig) -> Result<Self> {
        tokio::fs::create_dir_all(&config.data_dir).await?;
        if let Some(parent) = config.database_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let writer = connect_pool(&config.database_path, 1, config.busy_timeout).await?;
        reject_too_new_schema(&writer).await?;
        MIGRATOR.run(&writer).await?;
        sqlx::query("INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('schema_version', ?1)")
            .bind(CURRENT_SCHEMA_VERSION)
            .execute(&writer)
            .await?;

        let reader = connect_pool(
            &config.database_path,
            config.read_max_connections,
            config.busy_timeout,
        )
        .await?;

        Ok(Self {
            writer,
            reader,
            database_path: config.database_path,
            data_dir: config.data_dir,
            transcript_spill_bytes: config.transcript_spill_bytes,
        })
    }

    pub fn writer_pool(&self) -> &SqlitePool {
        &self.writer
    }

    pub fn reader_pool(&self) -> &SqlitePool {
        &self.reader
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn transcript_spill_bytes(&self) -> i64 {
        self.transcript_spill_bytes
    }

    pub fn backends(&self) -> repos::BackendsRepo<'_> {
        repos::BackendsRepo::new(self)
    }

    pub fn projects(&self) -> repos::ProjectsRepo<'_> {
        repos::ProjectsRepo::new(self)
    }

    pub fn sessions(&self) -> repos::SessionsRepo<'_> {
        repos::SessionsRepo::new(self)
    }

    pub fn crons(&self) -> repos::CronsRepo<'_> {
        repos::CronsRepo::new(self)
    }

    pub fn runs(&self) -> repos::RunsRepo<'_> {
        repos::RunsRepo::new(self)
    }

    pub fn chunks(&self) -> repos::ChunksRepo<'_> {
        repos::ChunksRepo::new(self)
    }

    pub fn settings(&self) -> repos::SettingsRepo<'_> {
        repos::SettingsRepo::new(self)
    }

    pub async fn close(&self) {
        self.reader.close().await;
        self.writer.close().await;
    }

    /// Create a consistent SQLite snapshot with the Online Backup API.
    ///
    /// This deliberately does not copy the WAL/SHM sidecars; SQLite
    /// materializes a standalone destination database that can be used as a
    /// fresh `lad.sqlite` in another state directory.
    pub async fn backup_to(&self, output: impl Into<PathBuf>) -> Result<()> {
        Self::backup_path_to(self.database_path.clone(), output).await
    }

    /// Create a consistent snapshot from an arbitrary SQLite source path.
    ///
    /// Used by `lad backup` so the CLI can back up a live daemon database
    /// without opening `Storage` (which would run migrations and create a
    /// second writer pool).
    pub async fn backup_path_to(
        source: impl Into<PathBuf>,
        output: impl Into<PathBuf>,
    ) -> Result<()> {
        let source = source.into();
        let output = output.into();
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if equivalent_paths(&source, &output).await? {
            return Err(StorageError::BackupSamePath(output.display().to_string()));
        }
        tokio::task::spawn_blocking(move || -> Result<()> {
            let source = rusqlite::Connection::open(source)?;
            let mut destination = rusqlite::Connection::open(output)?;
            let backup = rusqlite::backup::Backup::new(&source, &mut destination)?;
            backup.run_to_completion(64, std::time::Duration::from_millis(10), None)?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}

async fn connect_pool(
    path: &Path,
    max_connections: u32,
    busy_timeout: Duration,
) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(busy_timeout)
        .disable_statement_logging();

    SqlitePoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .map_err(Into::into)
}

async fn reject_too_new_schema(pool: &SqlitePool) -> Result<()> {
    let has_schema_meta: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
    )
    .fetch_one(pool)
    .await?;
    if has_schema_meta == 0 {
        return Ok(());
    }

    let Some(found) = sqlx::query_scalar::<_, String>(
        "SELECT value FROM schema_meta WHERE key = 'schema_version'",
    )
    .fetch_optional(pool)
    .await?
    else {
        return Ok(());
    };

    if version_gt(&found, CURRENT_SCHEMA_VERSION) {
        return Err(StorageError::SchemaTooNew {
            found,
            supported: CURRENT_SCHEMA_VERSION.to_string(),
        });
    }

    Ok(())
}

async fn equivalent_paths(left: &Path, right: &Path) -> Result<bool> {
    let left = tokio::fs::canonicalize(left).await?;
    let right = if tokio::fs::metadata(right).await.is_ok() {
        tokio::fs::canonicalize(right).await?
    } else if let Some(parent) = right.parent() {
        let parent = tokio::fs::canonicalize(parent).await?;
        parent.join(right.file_name().unwrap_or_default())
    } else {
        right.to_path_buf()
    };
    Ok(left == right)
}

fn version_gt(found: &str, supported: &str) -> bool {
    let found_num = found.parse::<u64>();
    let supported_num = supported.parse::<u64>();
    match (found_num, supported_num) {
        (Ok(found), Ok(supported)) => found > supported,
        _ => found > supported,
    }
}
