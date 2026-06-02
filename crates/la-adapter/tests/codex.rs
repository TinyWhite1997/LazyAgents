//! `la-adapter` integration tests for the Codex CLI adapter against
//! `mock-cli` (flavor=codex). Mirrors `tests/adapter.rs` and covers the
//! WEK-24 / M2.1 deliverables:
//!
//! - `probe`: Available, NotInstalled, Unauthenticated, Error
//! - `spawn_spec`: NullSink+prompt → `exec --json --cd <cwd> <prompt>`,
//!   interactive has no `exec`
//! - `encode_user_input` ends in `\n`
//! - `discover()` reads nested `sessions/YYYY/MM/DD/rollout-*.jsonl`
//!   and filters by `project_root`
//! - `graceful_stop()` is signal-only and ends in Kill

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use la_adapter::codex::{CodexAdapter, SESSIONS_DIR_ENV};
use la_adapter::{
    AgentAdapter, DiscoverHints, ProbeResult, SpawnRequest, StdinMode, StopAction, StopSignal,
};
use tokio::sync::Mutex;

fn mock_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock-cli"))
}

/// Probe tests mutate `MOCK_CLI_MODE` / `MOCK_CLI_FLAVOR` — serialize
/// access so cargo test's default thread-per-test parallelism doesn't
/// race them against each other (or against `tests/adapter.rs`'s
/// claude-flavored runs in the same process). `tokio::sync::Mutex` so
/// the guard can be held across `.await` cleanly (no
/// `clippy::await_holding_lock`).
static MOCK_ENV: Mutex<()> = Mutex::const_new(());

fn with_codex_flavor() {
    std::env::set_var("MOCK_CLI_FLAVOR", "codex");
}

fn clear_flavor_and_mode() {
    std::env::remove_var("MOCK_CLI_FLAVOR");
    std::env::remove_var("MOCK_CLI_MODE");
}

#[tokio::test]
async fn probe_against_mock_cli_returns_available() {
    let _g = MOCK_ENV.lock().await;
    clear_flavor_and_mode();
    with_codex_flavor();
    let adapter = CodexAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    clear_flavor_and_mode();

    match result {
        ProbeResult::Available { version } => {
            assert!(
                version.starts_with("0.135"),
                "expected 0.135.x style version, got {version:?}"
            );
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_missing_binary_as_not_installed() {
    let bogus = PathBuf::from("/this/path/should/not/exist/codex-xyz-9999");
    let adapter = CodexAdapter::with_program(bogus);
    match adapter.probe().await {
        ProbeResult::NotInstalled { hint } => {
            assert!(hint.contains("codex-xyz-9999"), "hint: {hint}");
        }
        other => panic!("expected NotInstalled, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_unauthenticated_via_stderr() {
    let _g = MOCK_ENV.lock().await;
    clear_flavor_and_mode();
    with_codex_flavor();
    std::env::set_var("MOCK_CLI_MODE", "unauth");
    let adapter = CodexAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    clear_flavor_and_mode();

    match result {
        ProbeResult::Unauthenticated { docs_url } => {
            assert!(docs_url.starts_with("https://"), "docs_url: {docs_url}");
        }
        other => panic!("expected Unauthenticated, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_garbage_output_as_error() {
    let _g = MOCK_ENV.lock().await;
    clear_flavor_and_mode();
    with_codex_flavor();
    std::env::set_var("MOCK_CLI_MODE", "garbage");
    let adapter = CodexAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    clear_flavor_and_mode();

    match result {
        ProbeResult::Error { detail } => {
            assert!(detail.contains("unrecognized"), "detail: {detail}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Older / unknown codex builds may not implement `login status` at all
/// — that subcommand will exit non-zero with NO "not logged in" keyword.
/// The secondary auth probe must NOT misreport that as `Unauthenticated`;
/// `--version` succeeded, so the user is `Available`.
#[tokio::test]
async fn probe_keeps_available_when_login_status_unsupported() {
    let _g = MOCK_ENV.lock().await;
    clear_flavor_and_mode();
    with_codex_flavor();
    std::env::set_var("MOCK_CLI_MODE", "login_unsupported");
    let adapter = CodexAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    clear_flavor_and_mode();

    match result {
        ProbeResult::Available { version } => {
            assert!(
                version.starts_with("0.135"),
                "expected 0.135.x version, got {version:?}"
            );
        }
        other => {
            panic!("login status unsupported must not be misreported as unauth — got {other:?}")
        }
    }
}

#[test]
fn spawn_spec_nullsink_emits_exec_json_cd_prompt_in_order() {
    let adapter = CodexAdapter::new();
    let cwd = PathBuf::from("/tmp/wt");
    let mut req = SpawnRequest::new(&cwd);
    req.stdin_mode = StdinMode::NullSink;
    req.prompt = Some("say hi".into());
    let spec = adapter.spawn_spec(&req).expect("spec");

    // The adapter prepends exec args; extra_args (none here) would
    // append after, so the first five slots are the contract.
    assert_eq!(
        spec.args,
        vec![
            OsString::from("exec"),
            OsString::from("--json"),
            OsString::from("--cd"),
            OsString::from(cwd.as_os_str()),
            OsString::from("say hi"),
        ]
    );
}

#[test]
fn spawn_spec_interactive_has_no_exec_arg() {
    let adapter = CodexAdapter::new();
    let req = SpawnRequest::new("/tmp/wt");
    let spec = adapter.spawn_spec(&req).expect("spec");
    assert!(
        !spec.args.iter().any(|a| a == &OsString::from("exec")),
        "interactive should not include `exec`, got {:?}",
        spec.args
    );
    assert_eq!(spec.stdin_mode, StdinMode::Pty);
}

#[test]
fn spawn_spec_honours_program_override() {
    let mut req = SpawnRequest::new("/tmp/wt");
    req.program_override = Some(PathBuf::from("/custom/codex"));
    let spec = CodexAdapter::new().spawn_spec(&req).expect("spec");
    assert_eq!(spec.program, PathBuf::from("/custom/codex"));
}

#[test]
fn encode_user_input_appends_lf() {
    let adapter = CodexAdapter::new();
    let bytes = adapter.encode_user_input("hi");
    assert_eq!(&*bytes, b"hi\n");
}

#[test]
fn graceful_stop_sequence_shape() {
    let seq = CodexAdapter::new().graceful_stop();
    let kinds: Vec<&'static str> = seq
        .0
        .iter()
        .map(|a| match a {
            StopAction::SendInput(_) => "send",
            StopAction::AwaitExit(_) => "await",
            StopAction::Signal(StopSignal::Terminate) => "term",
            StopAction::Signal(StopSignal::Interrupt) => "int",
            StopAction::Signal(StopSignal::Kill) => "kill",
        })
        .collect();
    assert_eq!(kinds, vec!["term", "await", "kill"]);
}

/// `discover()` walks the nested rollout layout and surfaces each
/// session's external id + cwd hint. `project_root` filtering narrows
/// the result set without touching the user's real home.
#[tokio::test]
async fn discover_reads_nested_rollout_layout() {
    let _g = MOCK_ENV.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");

    // Two projects, each with one rollout.
    let proj_a = tmp.path().join("proj-a");
    let proj_b = tmp.path().join("proj-b");
    fs::create_dir_all(&proj_a).unwrap();
    fs::create_dir_all(&proj_b).unwrap();
    let canon_a = fs::canonicalize(&proj_a).unwrap();

    let day = root.join("2026").join("06").join("02");
    fs::create_dir_all(&day).unwrap();

    let file_a = day.join("rollout-2026-06-02T10-00-00-019e0000-0000-0000-0000-000000000000.jsonl");
    let file_b = day.join("rollout-2026-06-02T11-00-00-019e0000-0000-0000-0000-000000000001.jsonl");

    fs::write(
        &file_a,
        format!(
            "{{\"timestamp\":\"2026-06-02T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019e0000-0000-0000-0000-000000000000\",\"timestamp\":\"2026-06-02T10:00:00Z\",\"cwd\":\"{}\",\"originator\":\"codex_cli_rs\",\"cli_version\":\"0.135.0\"}}}}\n{{\"type\":\"task_started\"}}\n",
            proj_a.display()
        ),
    )
    .unwrap();
    fs::write(
        &file_b,
        format!(
            "{{\"timestamp\":\"2026-06-02T11:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"019e0000-0000-0000-0000-000000000001\",\"timestamp\":\"2026-06-02T11:00:00Z\",\"cwd\":\"{}\",\"originator\":\"codex_cli_rs\",\"cli_version\":\"0.135.0\"}}}}\n",
            proj_b.display()
        ),
    )
    .unwrap();

    // Tolerated noise: a malformed first line should be silently
    // skipped rather than panic / abort the whole walk.
    let bad = day.join("rollout-broken.jsonl");
    fs::write(&bad, b"not json at all\n").unwrap();

    std::env::set_var(SESSIONS_DIR_ENV, &root);
    let adapter = CodexAdapter::new();

    // Unfiltered: both well-formed entries surface.
    let all = adapter
        .discover(&DiscoverHints::default())
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);

    assert_eq!(all.len(), 2, "expected 2 sessions, got {all:?}");
    let ids: Vec<&str> = all.iter().map(|s| s.external_id.as_str()).collect();
    assert!(ids.contains(&"019e0000-0000-0000-0000-000000000000"));
    assert!(ids.contains(&"019e0000-0000-0000-0000-000000000001"));

    // Filtered by project root: only proj-a comes back.
    std::env::set_var(SESSIONS_DIR_ENV, &root);
    let hints = DiscoverHints {
        project_root: Some(canon_a.clone()),
    };
    let filtered = adapter.discover(&hints).await.expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);

    assert_eq!(filtered.len(), 1);
    assert_eq!(
        filtered[0].external_id,
        "019e0000-0000-0000-0000-000000000000"
    );
}

#[tokio::test]
async fn discover_returns_empty_when_root_missing() {
    let _g = MOCK_ENV.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let absent = tmp.path().join("does-not-exist");
    std::env::set_var(SESSIONS_DIR_ENV, &absent);
    let out = CodexAdapter::new()
        .discover(&DiscoverHints::default())
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);
    assert!(out.is_empty());
}
