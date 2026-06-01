//! SQLite storage for LazyAgents.
//!
//! This crate owns the daemon-side SQLite schema, sqlx migrations, WAL pool
//! setup, and typed repositories. It intentionally has no IPC or PTY logic.

mod models;
mod repos;
mod storage;

pub use models::*;
pub use repos::*;
pub use storage::{Storage, StorageConfig};

pub const MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub const CURRENT_SCHEMA_VERSION: &str = "1";
pub const DEFAULT_TRANSCRIPT_SPILL_BYTES: i64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("missing session: {0}")]
    MissingSession(String),
    #[error("missing project: {0}")]
    MissingProject(String),
    #[error("busy after {attempts} attempts")]
    Busy { attempts: usize },
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}
