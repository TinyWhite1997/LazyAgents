//! `la-adapter::claude` discover-side integration tests. The probe /
//! spawn coverage already lives in `tests/adapter.rs`; this file
//! exercises the WEK-26 / M2.3 `discover()` impl against a fixture
//! `~/.claude/projects`-shaped tree.

use std::fs;

use la_adapter::claude::{ClaudeAdapter, SESSIONS_DIR_ENV};
use la_adapter::{AgentAdapter, DiscoverHints};
use tokio::sync::Mutex;

/// Serialise `CLAUDE_SESSIONS_DIR` mutations so concurrent tests don't
/// see each other's env state.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn discover_walks_projects_layout_and_filters() {
    let _g = ENV_LOCK.lock().await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();

    let proj_a = tmp.path().join("proj-a");
    let proj_b = tmp.path().join("proj-b");
    fs::create_dir_all(&proj_a).unwrap();
    fs::create_dir_all(&proj_b).unwrap();
    let canon_a = std::fs::canonicalize(&proj_a).unwrap();

    // Claude lays sessions out as `<root>/<encoded-cwd>/<uuid>.jsonl`.
    // We don't actually depend on the encoding scheme — the discover
    // path just recurses one level deep and reads `cwd` out of the
    // first JSONL line.
    let enc_a = root.join("encoded-proj-a");
    let enc_b = root.join("encoded-proj-b");
    fs::create_dir_all(&enc_a).unwrap();
    fs::create_dir_all(&enc_b).unwrap();

    let file_a = enc_a.join("00000000-0000-0000-0000-000000000aaa.jsonl");
    fs::write(
        &file_a,
        format!(
            "{{\"type\":\"session_start\",\"session_id\":\"00000000-0000-0000-0000-000000000aaa\",\"cwd\":\"{}\",\"timestamp\":\"2026-06-01T09:00:00Z\"}}\n",
            proj_a.display()
        ),
    )
    .unwrap();

    let file_b = enc_b.join("00000000-0000-0000-0000-000000000bbb.jsonl");
    fs::write(
        &file_b,
        format!(
            "{{\"type\":\"session_start\",\"sessionId\":\"00000000-0000-0000-0000-000000000bbb\",\"workingDir\":\"{}\",\"createdAt\":\"2026-06-01T10:00:00Z\"}}\n",
            proj_b.display()
        ),
    )
    .unwrap();

    // A malformed line at the top should be skipped, not aborted.
    let file_c = enc_b.join("00000000-0000-0000-0000-000000000ccc.jsonl");
    fs::write(
        &file_c,
        format!(
            "not json\n{{\"type\":\"session_start\",\"id\":\"00000000-0000-0000-0000-000000000ccc\",\"cwd\":\"{}\"}}\n",
            proj_b.display()
        ),
    )
    .unwrap();

    std::env::set_var(SESSIONS_DIR_ENV, &root);
    let adapter = ClaudeAdapter::new();
    let all = adapter
        .discover(&DiscoverHints::default())
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);

    assert_eq!(all.len(), 3, "expected 3 sessions, got {all:?}");
    let ids: Vec<&str> = all.iter().map(|s| s.external_id.as_str()).collect();
    assert!(ids.contains(&"00000000-0000-0000-0000-000000000aaa"));
    assert!(ids.contains(&"00000000-0000-0000-0000-000000000bbb"));
    assert!(ids.contains(&"00000000-0000-0000-0000-000000000ccc"));

    // Every entry must surface a real on-disk transcript path, and at
    // least one must propagate the backend's own RFC3339 timestamp
    // (the others fall back to the file's mtime, which is fine).
    for s in &all {
        let p = s.external_path.as_ref().expect("external_path");
        assert!(p.exists(), "external_path {} must exist", p.display());
        assert!(s.created_at.is_some(), "created_at must be filled in");
    }
    assert!(all
        .iter()
        .any(|s| s.created_at.as_deref() == Some("2026-06-01T09:00:00Z")));

    // Filter by project root: only proj-a comes back.
    std::env::set_var(SESSIONS_DIR_ENV, &root);
    let hints = DiscoverHints {
        project_root: Some(canon_a.clone()),
        source_path_override: None,
    };
    let filtered = adapter.discover(&hints).await.expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);

    assert_eq!(filtered.len(), 1);
    assert_eq!(
        filtered[0].external_id,
        "00000000-0000-0000-0000-000000000aaa"
    );
}

#[tokio::test]
async fn discover_returns_empty_when_root_missing() {
    let _g = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let absent = tmp.path().join("does-not-exist");
    std::env::set_var(SESSIONS_DIR_ENV, &absent);
    let out = ClaudeAdapter::new()
        .discover(&DiscoverHints::default())
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);
    assert!(out.is_empty());
}

#[tokio::test]
async fn discover_honours_source_path_override() {
    let _g = ENV_LOCK.lock().await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let custom_root = tmp.path().to_path_buf();
    let session_dir = custom_root.join("encoded-anywhere");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("00000000-0000-0000-0000-000000000ddd.jsonl"),
        b"{\"type\":\"session_start\",\"session_id\":\"00000000-0000-0000-0000-000000000ddd\"}\n",
    )
    .unwrap();

    // CLAUDE_SESSIONS_DIR points elsewhere — the override must win.
    let other = tmp.path().join("other-root-empty");
    fs::create_dir_all(&other).unwrap();
    std::env::set_var(SESSIONS_DIR_ENV, &other);
    let out = ClaudeAdapter::new()
        .discover(&DiscoverHints {
            project_root: None,
            source_path_override: Some(custom_root.clone()),
        })
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].external_id, "00000000-0000-0000-0000-000000000ddd");
}
