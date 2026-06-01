use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, SqlitePool};

use crate::{repos, Result, CURRENT_SCHEMA_VERSION, MIGRATOR};

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
