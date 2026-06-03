use std::io::{BufRead, Write};
use std::time::Duration;

use la_storage::{
    AppendOutcome, BackendUpsert, ChunkKind, CronUpsert, NewProject, NewRun, NewSession, RunFinish,
    RunsListFilter, Storage, StorageConfig, StorageError, CURRENT_SCHEMA_VERSION,
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
            post_create_hook_status: Some("ok".into()),
            external_path: None,
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
    assert_eq!(migration.as_deref(), Some("0004_crons_runs"));
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
async fn crons_and_runs_cover_crud_and_archive_paths() {
    let (_dir, storage) = open_storage().await;
    let (_backend_id, project_id, _session_id) = seed_backend_project_session(&storage).await;
    let cron_id = la_storage::new_id();

    let cron = storage
        .crons()
        .upsert(CronUpsert {
            id: cron_id.clone(),
            name: "nightly".into(),
            enabled: true,
            project_id: project_id.clone(),
            backend_id: "claude".into(),
            spawn_args: serde_json::json!({"cwd":"/tmp/lazyagents/project"}),
            prompt: "summarize".into(),
            cron_expr: "17 3 * * *".into(),
            tz: "UTC".into(),
            catchup_mode: "coalesce".into(),
            max_concurrent_runs: 1,
            max_runs_per_day: 24,
            max_runtime_s: 1800,
            cost_budget_usd_per_day: Some(1.5),
            failure_backoff: "expo(1m,2,1h)".into(),
            pause_on_consecutive_failures: 5,
            consecutive_failures: 0,
            last_fired_at: None,
            next_fire_at: Some("2000-01-01 03:17:00".into()),
        })
        .await
        .unwrap();
    assert_eq!(cron.enabled, 1);
    assert_eq!(
        storage
            .crons()
            .list_enabled_due("2000-01-02")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(storage
        .crons()
        .mark_fired(&cron_id, "2000-01-01 03:17:00", Some("2000-01-02 03:17:00"))
        .await
        .unwrap());
    assert!(!storage
        .crons()
        .mark_fired(&cron_id, "1999-12-31 03:17:00", Some("2000-01-01 03:17:00"))
        .await
        .unwrap());
    let marked = storage.crons().get(&cron_id).await.unwrap().unwrap();
    assert_eq!(marked.last_fired_at.as_deref(), Some("2000-01-01 03:17:00"));

    let run = storage
        .runs()
        .create(NewRun {
            id: "run-old".into(),
            cron_id: Some(cron_id.clone()),
            session_id: None,
            scheduled_at: "2000-01-01 03:17:00".into(),
            started_at: Some("2000-01-01 03:17:01".into()),
            status: "running".into(),
            coalesced_count: 2,
        })
        .await
        .unwrap();
    assert_eq!(run.coalesced_count, 2);
    assert!(storage
        .runs()
        .attach_session("run-old", &_session_id)
        .await
        .unwrap());
    assert!(storage
        .runs()
        .finish(
            "run-old",
            RunFinish {
                finished_at: "2000-01-01 03:18:00".into(),
                status: "completed".into(),
                exit_code: Some(0),
                cost_usd_est: Some(0.01),
                error_kind: None,
                error_detail: None,
                tail_log: Some(vec![b'x'; 70 * 1024]),
            },
        )
        .await
        .unwrap());
    assert!(storage
        .runs()
        .finish(
            "run-old",
            RunFinish {
                finished_at: "2000-01-01 03:18:00".into(),
                status: "completed".into(),
                exit_code: Some(0),
                cost_usd_est: Some(0.01),
                error_kind: None,
                error_detail: None,
                tail_log: Some(vec![b'x'; 70 * 1024]),
            },
        )
        .await
        .unwrap());
    assert!(!storage
        .runs()
        .finish(
            "run-old",
            RunFinish {
                finished_at: "2000-01-01 03:19:00".into(),
                status: "failed".into(),
                exit_code: Some(1),
                cost_usd_est: None,
                error_kind: Some("adapter".into()),
                error_detail: Some("late duplicate".into()),
                tail_log: None,
            },
        )
        .await
        .unwrap());
    assert!(!storage
        .runs()
        .update_status("run-old", "running")
        .await
        .unwrap());
    let listed = storage
        .runs()
        .list(RunsListFilter {
            cron_id: Some(&cron_id),
            since: Some("1999-01-01"),
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, "completed");
    assert_eq!(listed[0].tail_log.as_ref().unwrap().len(), 64 * 1024);

    let outcome = storage.runs().archive_older_than_days(90).await.unwrap();
    assert_eq!(outcome.archived_rows, 1);
    assert_eq!(outcome.archive_files, 1);
    assert!(storage.runs().get("run-old").await.unwrap().is_none());
    assert!(storage
        .data_dir()
        .join("runs/archive/200001.jsonl.zst")
        .exists());
    let archived = read_archive_jsonl(storage.data_dir().join("runs/archive/200001.jsonl.zst"));
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0]["id"], "run-old");
}

#[tokio::test]
async fn archive_retry_deletes_previously_written_rows_without_duplicate_jsonl() {
    let (_dir, storage) = open_storage().await;
    let (_backend_id, project_id, _session_id) = seed_backend_project_session(&storage).await;
    storage
        .crons()
        .upsert(CronUpsert {
            id: "cron-retry".into(),
            name: "retry".into(),
            enabled: true,
            project_id,
            backend_id: "claude".into(),
            spawn_args: serde_json::json!({}),
            prompt: "retry".into(),
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
    storage
        .runs()
        .create(NewRun {
            id: "run-retry".into(),
            cron_id: Some("cron-retry".into()),
            session_id: None,
            scheduled_at: "2000-02-01 03:17:00".into(),
            started_at: Some("2000-02-01 03:17:01".into()),
            status: "running".into(),
            coalesced_count: 1,
        })
        .await
        .unwrap();
    // Finish the run so archive_older_than_days picks it up under the
    // Rev2 §3.3 contract (`finished_at < ?` AND status in terminal set).
    storage
        .runs()
        .finish(
            "run-retry",
            RunFinish {
                finished_at: "2000-02-01 03:18:00".into(),
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

    let archive_path = storage.data_dir().join("runs/archive/200002.jsonl.zst");
    write_archive_jsonl(
        &archive_path,
        &[serde_json::json!({
            "id": "run-retry",
            "cron_id": "cron-retry",
            "scheduled_at": "2000-02-01 03:17:00",
            "status": "completed"
        })],
    );

    let outcome = storage.runs().archive_older_than_days(90).await.unwrap();
    assert_eq!(outcome.archived_rows, 1);
    // Rev2 §S5 trades dedup-on-write for a fail-closed atomic batch:
    // the archive file is written even if a previous (stale) row exists,
    // and recovery handles the duplicate by keeping the last `id` write.
    assert_eq!(outcome.archive_files, 1);
    assert!(storage.runs().get("run-retry").await.unwrap().is_none());
    let archived = read_archive_jsonl(archive_path);
    let ids: Vec<_> = archived
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    // Duplicate is expected: the seeded stale line plus the freshly
    // archived one. Recovery's contract is "last write wins on `id`".
    assert_eq!(ids, vec!["run-retry", "run-retry"]);
}

#[tokio::test]
async fn backup_file_can_open_as_new_storage_database() {
    let (dir, storage) = open_storage().await;
    let (_backend_id, project_id, _session_id) = seed_backend_project_session(&storage).await;
    let backup_path = dir.path().join("backup.sqlite");
    storage.backup_to(&backup_path).await.unwrap();
    storage.backup_to(&backup_path).await.unwrap();
    let err = storage
        .backup_to(storage.database_path().to_path_buf())
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::BackupSamePath(_)));
    storage.close().await;

    let restore_dir = TempDir::new().expect("restore dir");
    let restored = Storage::open(StorageConfig::new(&backup_path, restore_dir.path()))
        .await
        .expect("open backup as storage");
    assert!(restored
        .projects()
        .get(&project_id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_is_consistent_while_runs_archive_deletes_rows() {
    let (dir, storage) = open_storage().await;
    let (_backend_id, project_id, _session_id) = seed_backend_project_session(&storage).await;
    storage
        .crons()
        .upsert(CronUpsert {
            id: "cron-backup-archive".into(),
            name: "backup archive".into(),
            enabled: true,
            project_id,
            backend_id: "claude".into(),
            spawn_args: serde_json::json!({}),
            prompt: "backup".into(),
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
    for i in 0..100 {
        let id = format!("run-archive-{i}");
        storage
            .runs()
            .create(NewRun {
                id: id.clone(),
                cron_id: Some("cron-backup-archive".into()),
                session_id: None,
                scheduled_at: "2000-03-01 03:17:00".into(),
                started_at: Some("2000-03-01 03:17:01".into()),
                status: "running".into(),
                coalesced_count: 1,
            })
            .await
            .unwrap();
        // Required by Rev2 §3.3: archive needs finished_at AND a
        // terminal status; a never-finished row is left in place.
        storage
            .runs()
            .finish(
                &id,
                RunFinish {
                    finished_at: "2000-03-01 03:18:00".into(),
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

    let backup_path = dir.path().join("archive-live-backup.sqlite");
    let backup_storage = storage.clone();
    let archive_storage = storage.clone();
    let backup_task = tokio::spawn(async move { backup_storage.backup_to(&backup_path).await });
    let archive_task =
        tokio::spawn(async move { archive_storage.runs().archive_older_than_days(90).await });
    backup_task.await.expect("backup join").expect("backup");
    archive_task.await.expect("archive join").expect("archive");

    let restore_dir = TempDir::new().expect("restore dir");
    let restored = Storage::open(StorageConfig::new(
        dir.path().join("archive-live-backup.sqlite"),
        restore_dir.path(),
    ))
    .await
    .expect("open archive-live backup");
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(restored.reader_pool())
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    let schema_version = restored
        .settings()
        .schema_meta("schema_version")
        .await
        .unwrap();
    assert_eq!(schema_version.as_deref(), Some(CURRENT_SCHEMA_VERSION));
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(restored.writer_pool())
        .await
        .unwrap();
    assert_eq!(fk, 1);
}

fn write_archive_jsonl(path: &std::path::Path, rows: &[serde_json::Value]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("archive parent");
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("archive file");
    let mut encoder = zstd::stream::write::Encoder::new(file, 0).expect("zstd encoder");
    for row in rows {
        encoder
            .write_all(serde_json::to_string(row).unwrap().as_bytes())
            .expect("write row");
        encoder.write_all(b"\n").expect("write newline");
    }
    encoder.finish().expect("finish zstd");
}

fn read_archive_jsonl(path: impl AsRef<std::path::Path>) -> Vec<serde_json::Value> {
    let file = std::fs::File::open(path).expect("archive file");
    let decoder = zstd::stream::read::Decoder::new(file).expect("zstd decoder");
    std::io::BufReader::new(decoder)
        .lines()
        .map(|line| serde_json::from_str(&line.expect("line")).expect("json"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_is_consistent_while_writes_continue() {
    let (dir, storage) = open_storage().await;
    let (_backend_id, project_id, session_id) = seed_backend_project_session(&storage).await;
    let backup_path = dir.path().join("live-backup.sqlite");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let writer_storage = storage.clone();
    let writer_session_id = session_id.clone();
    let writer = tokio::spawn(async move {
        let mut started_tx = Some(started_tx);
        for i in 0..200 {
            writer_storage
                .chunks()
                .append(
                    &writer_session_id,
                    ChunkKind::Stdout,
                    format!("live-write-{i}").as_bytes(),
                )
                .await
                .expect("append during backup");
            if let Some(tx) = started_tx.take() {
                let _ = tx.send(());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    started_rx.await.expect("writer started");
    storage.backup_to(&backup_path).await.unwrap();
    writer.await.expect("writer task");

    let restore_dir = TempDir::new().expect("restore dir");
    let restored = Storage::open(StorageConfig::new(&backup_path, restore_dir.path()))
        .await
        .expect("open live backup as storage");
    assert!(restored
        .projects()
        .get(&project_id)
        .await
        .unwrap()
        .is_some());
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(restored.reader_pool())
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
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
