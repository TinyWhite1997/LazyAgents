use std::time::Duration;

use la_storage::{
    AppendOutcome, BackendUpsert, ChunkKind, NewProject, NewSession, Storage, StorageConfig,
    StorageError, CURRENT_SCHEMA_VERSION,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Connection, Executor, SqliteConnection};
use tempfile::TempDir;

async fn open_storage() -> (TempDir, Storage) {
    let dir = TempDir::new().expect("tempdir");
    let storage = Storage::open(StorageConfig::for_test(dir.path()))
        .await
        .expect("open storage");
    (dir, storage)
}

async fn seed_backend_project_session(storage: &Storage) -> (String, String, String) {
    storage
        .backends()
        .upsert(BackendUpsert {
            id: "claude",
            display_name: "Claude Code",
            version: Some("2.1.0"),
            available: true,
        })
        .await
        .expect("backend");

    let project_id = la_storage::new_id();
    storage
        .projects()
        .create(NewProject {
            id: project_id.clone(),
            root_path: "/tmp/lazyagents/project".into(),
            display_name: "project".into(),
            vcs: Some("git".into()),
        })
        .await
        .expect("project");

    let session_id = la_storage::new_id();
    storage
        .sessions()
        .create(NewSession {
            id: session_id.clone(),
            project_id: project_id.clone(),
            backend_id: "claude".into(),
            external_id: Some("ext-1".into()),
            title: Some("initial".into()),
            state: "running".into(),
            pid: Some(4242),
            worktree_path: Some("/tmp/lazyagents/wt".into()),
            worktree_branch: Some("la/test".into()),
            base_branch: Some("main".into()),
            spawn_args: serde_json::json!({"args":["--verbose"]}),
            origin: "user".into(),
        })
        .await
        .expect("session");

    ("claude".into(), project_id, session_id)
}

#[tokio::test]
async fn migrations_enable_wal_and_schema_meta() {
    let (_dir, storage) = open_storage().await;

    let schema_version = storage
        .settings()
        .schema_meta("schema_version")
        .await
        .expect("schema meta");
    assert_eq!(schema_version.as_deref(), Some(CURRENT_SCHEMA_VERSION));

    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(storage.writer_pool())
        .await
        .expect("journal mode");
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(storage.writer_pool())
        .await
        .expect("foreign keys");
    assert_eq!(fk, 1);

    let migration: Option<String> = storage
        .settings()
        .schema_meta("migration")
        .await
        .expect("migration meta");
    assert_eq!(migration.as_deref(), Some("0001_initial"));
}

#[tokio::test]
async fn open_rejects_schema_newer_than_supported() {
    let dir = TempDir::new().expect("tempdir");
    let config = StorageConfig::for_test(dir.path());
    let storage = Storage::open(config.clone()).await.expect("open storage");
    sqlx::query("INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('schema_version', '99')")
        .execute(storage.writer_pool())
        .await
        .expect("write newer schema");
    storage.close().await;
    drop(storage);

    let err = match Storage::open(config).await {
        Ok(_) => panic!("newer schema must be rejected"),
        Err(err) => err,
    };
    match err {
        StorageError::SchemaTooNew { found, supported } => {
            assert_eq!(found, "99");
            assert_eq!(supported, CURRENT_SCHEMA_VERSION);
        }
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }
}

#[tokio::test]
async fn repositories_cover_crud_paths() {
    let (_dir, storage) = open_storage().await;
    let (_backend_id, project_id, session_id) = seed_backend_project_session(&storage).await;

    let backend = storage.backends().get("claude").await.unwrap().unwrap();
    assert_eq!(backend.display_name, "Claude Code");
    assert_eq!(storage.backends().list().await.unwrap().len(), 1);

    let project = storage.projects().get(&project_id).await.unwrap().unwrap();
    assert_eq!(project.vcs.as_deref(), Some("git"));
    assert!(storage
        .projects()
        .update_display_name(&project_id, "renamed")
        .await
        .unwrap());
    let by_root = storage
        .projects()
        .get_by_root_path("/tmp/lazyagents/project")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_root.display_name, "renamed");

    let sessions = storage
        .sessions()
        .list_by_project(&project_id, false)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(storage
        .sessions()
        .update_state(&session_id, "exited", Some(0))
        .await
        .unwrap());
    assert!(storage.sessions().archive(&session_id).await.unwrap());
    assert!(storage
        .sessions()
        .list_by_project(&project_id, false)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        storage
            .sessions()
            .list_by_project(&project_id, true)
            .await
            .unwrap()
            .len(),
        1
    );

    let outcome = storage
        .chunks()
        .append(&session_id, ChunkKind::Stdout, b"hello")
        .await
        .unwrap();
    assert_eq!(outcome, AppendOutcome::StoredInDb { seq: 1 });
    let chunks = storage.chunks().list(&session_id).await.unwrap();
    assert_eq!(chunks[0].data, b"hello");

    storage.settings().set("theme", "dark").await.unwrap();
    assert_eq!(
        storage.settings().get("theme").await.unwrap().as_deref(),
        Some("dark")
    );
    assert!(storage.settings().delete("theme").await.unwrap());
    assert!(storage.sessions().delete(&session_id).await.unwrap());
    assert!(storage.projects().delete(&project_id).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_allocate_unique_sequences() {
    let (_dir, storage) = open_storage().await;
    let (_backend_id, _project_id, session_id) = seed_backend_project_session(&storage).await;

    let mut tasks = Vec::new();
    for i in 0..1000 {
        let storage = storage.clone();
        let session_id = session_id.clone();
        tasks.push(tokio::spawn(async move {
            let kind = if i % 2 == 0 {
                ChunkKind::Stdout
            } else {
                ChunkKind::Stderr
            };
            storage
                .chunks()
                .append(&session_id, kind, format!("chunk-{i}").as_bytes())
                .await
        }));
    }

    for task in tasks {
        task.await.expect("join").expect("append");
    }

    let chunks = storage.chunks().list(&session_id).await.unwrap();
    assert_eq!(chunks.len(), 1000);
    for (idx, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.seq, (idx + 1) as i64);
    }
}

#[tokio::test]
async fn transcript_spills_to_external_file_after_threshold() {
    let dir = TempDir::new().expect("tempdir");
    let mut config = StorageConfig::for_test(dir.path());
    config.transcript_spill_bytes = 8;
    let storage = Storage::open(config).await.expect("open storage");
    let (_backend_id, _project_id, session_id) = seed_backend_project_session(&storage).await;

    assert_eq!(
        storage
            .chunks()
            .append(&session_id, ChunkKind::Stdout, b"1234")
            .await
            .unwrap(),
        AppendOutcome::StoredInDb { seq: 1 }
    );
    let spilled = storage
        .chunks()
        .append(&session_id, ChunkKind::Stderr, b"56789")
        .await
        .unwrap();

    let path = match spilled {
        AppendOutcome::SpilledToFile { seq, path } => {
            assert_eq!(seq, 2);
            path
        }
        other => panic!("expected spill, got {other:?}"),
    };
    assert!(std::path::Path::new(&path).exists());

    let chunks = storage.chunks().list(&session_id).await.unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].data, b"1234");
    assert_eq!(chunks[1].data, b"56789");

    let db_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_chunks")
        .fetch_one(storage.reader_pool())
        .await
        .unwrap();
    assert_eq!(db_count, 0);
}

#[tokio::test]
async fn rolled_back_spill_delete_does_not_duplicate_chunks_after_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let config = StorageConfig::for_test(dir.path());
    let storage = Storage::open(config.clone()).await.expect("open storage");
    let (_backend_id, _project_id, session_id) = seed_backend_project_session(&storage).await;
    storage
        .chunks()
        .append(&session_id, ChunkKind::Stdout, b"abc")
        .await
        .unwrap();

    let spill_dir = dir.path().join("sessions");
    tokio::fs::create_dir_all(&spill_dir).await.unwrap();
    tokio::fs::write(
        spill_dir.join(format!("{session_id}.log")),
        format!(
            "{{\"session_id\":\"{session_id}\",\"seq\":1,\"ts\":\"2026-06-01T00:00:00Z\",\"kind\":\"stdout\",\"data_base64\":\"YWJj\"}}\n"
        ),
    )
    .await
    .unwrap();

    let mut tx = storage.writer_pool().begin().await.unwrap();
    sqlx::query("DELETE FROM session_chunks WHERE session_id = ?1")
        .bind(&session_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    storage.close().await;
    drop(storage);

    let storage = Storage::open(config).await.expect("reopen storage");
    let chunks = storage.chunks().list(&session_id).await.unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].seq, 1);
    assert_eq!(chunks[0].data, b"abc");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_writer_retries_until_lock_clears() {
    let dir = TempDir::new().expect("tempdir");
    let mut config = StorageConfig::for_test(dir.path());
    config.busy_timeout = Duration::from_millis(20);
    let storage = Storage::open(config.clone()).await.expect("open storage");

    let options = SqliteConnectOptions::new()
        .filename(&config.database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(20));
    let mut blocker = SqliteConnection::connect_with(&options)
        .await
        .expect("blocker connect");
    blocker
        .execute("BEGIN IMMEDIATE")
        .await
        .expect("begin immediate");

    let writer = storage.clone();
    let task = tokio::spawn(async move { writer.settings().set("locked", "eventually").await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    blocker.execute("COMMIT").await.expect("commit");

    task.await.expect("join").expect("retry succeeds");
    assert_eq!(
        storage.settings().get("locked").await.unwrap().as_deref(),
        Some("eventually")
    );
}
