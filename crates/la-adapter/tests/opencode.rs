//! `la-adapter` integration tests for the OpenCode CLI adapter against
//! `mock-cli` (flavor=opencode). Mirrors `tests/codex.rs` and covers the
//! WEK-25 / M2.2 deliverables:
//!
//! - `probe`: Available, NotInstalled, Unauthenticated, Error,
//!   plus `auth list` unsupported = stays Available
//! - `spawn_spec`: NullSink+prompt → `run --format json --dir <cwd> <prompt>`,
//!   interactive has no `run`
//! - `encode_user_input` ends in `\n`
//! - `discover()` reads flat `sessions/*.json` and nested
//!   `sessions/<scope>/*.json`, filters by `project_root`, honours
//!   `OPENCODE_SESSIONS_DIR` override
//! - `graceful_stop()` is signal-only and ends in Kill

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use la_adapter::opencode::{OpencodeAdapter, SESSIONS_DIR_ENV};
use la_adapter::{
    AgentAdapter, DiscoverHints, ProbeResult, SpawnRequest, StdinMode, StopAction, StopSignal,
};
use tokio::sync::Mutex;

fn mock_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock-cli"))
}

/// Probe tests mutate `MOCK_CLI_MODE` / `MOCK_CLI_FLAVOR` — serialize
/// access so cargo test's default thread-per-test parallelism doesn't
/// race them against each other (or against `tests/adapter.rs` /
/// `tests/codex.rs` flavor switches in the same process). `tokio::sync::Mutex`
/// so the guard can be held across `.await` cleanly (no
/// `clippy::await_holding_lock`).
static MOCK_ENV: Mutex<()> = Mutex::const_new(());

fn with_opencode_flavor() {
    std::env::set_var("MOCK_CLI_FLAVOR", "opencode");
}

fn clear_flavor_and_mode() {
    std::env::remove_var("MOCK_CLI_FLAVOR");
    std::env::remove_var("MOCK_CLI_MODE");
}

#[tokio::test]
async fn probe_against_mock_cli_returns_available() {
    let _g = MOCK_ENV.lock().await;
    clear_flavor_and_mode();
    with_opencode_flavor();
    let adapter = OpencodeAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    clear_flavor_and_mode();

    match result {
        ProbeResult::Available { version } => {
            assert!(
                version.starts_with("1.2"),
                "expected 1.2.x style version, got {version:?}"
            );
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_missing_binary_as_not_installed() {
    let bogus = PathBuf::from("/this/path/should/not/exist/opencode-xyz-9999");
    let adapter = OpencodeAdapter::with_program(bogus);
    match adapter.probe().await {
        ProbeResult::NotInstalled { hint } => {
            assert!(hint.contains("opencode-xyz-9999"), "hint: {hint}");
        }
        other => panic!("expected NotInstalled, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_unauthenticated_via_stderr() {
    let _g = MOCK_ENV.lock().await;
    clear_flavor_and_mode();
    with_opencode_flavor();
    std::env::set_var("MOCK_CLI_MODE", "unauth");
    let adapter = OpencodeAdapter::with_program(mock_cli());
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
    with_opencode_flavor();
    std::env::set_var("MOCK_CLI_MODE", "garbage");
    let adapter = OpencodeAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    clear_flavor_and_mode();

    match result {
        ProbeResult::Error { detail } => {
            assert!(detail.contains("unrecognized"), "detail: {detail}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Older / unknown opencode builds may not implement `auth list` at
/// all — that subcommand will exit non-zero with NO "no credentials"
/// keyword. The secondary auth probe must NOT misreport that as
/// `Unauthenticated`; `--version` succeeded, so the user is
/// `Available`.
#[tokio::test]
async fn probe_keeps_available_when_auth_list_unsupported() {
    let _g = MOCK_ENV.lock().await;
    clear_flavor_and_mode();
    with_opencode_flavor();
    std::env::set_var("MOCK_CLI_MODE", "auth_unsupported");
    let adapter = OpencodeAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    clear_flavor_and_mode();

    match result {
        ProbeResult::Available { version } => {
            assert!(
                version.starts_with("1.2"),
                "expected 1.2.x version, got {version:?}"
            );
        }
        other => {
            panic!("auth list unsupported must not be misreported as unauth — got {other:?}")
        }
    }
}

#[test]
fn spawn_spec_nullsink_emits_run_format_json_dir_prompt_in_order() {
    let adapter = OpencodeAdapter::new();
    let cwd = PathBuf::from("/tmp/wt");
    let mut req = SpawnRequest::new(&cwd);
    req.stdin_mode = StdinMode::NullSink;
    req.prompt = Some("say hi".into());
    let spec = adapter.spawn_spec(&req).expect("spec");

    assert_eq!(
        spec.args,
        vec![
            OsString::from("run"),
            OsString::from("--format"),
            OsString::from("json"),
            OsString::from("--dir"),
            OsString::from(cwd.as_os_str()),
            OsString::from("say hi"),
        ]
    );
}

#[test]
fn spawn_spec_nullsink_without_cwd_skips_dir_flag() {
    let adapter = OpencodeAdapter::new();
    let mut req = SpawnRequest::new("");
    req.stdin_mode = StdinMode::NullSink;
    req.prompt = Some("hi".into());
    let spec = adapter.spawn_spec(&req).expect("spec");
    assert_eq!(
        spec.args,
        vec![
            OsString::from("run"),
            OsString::from("--format"),
            OsString::from("json"),
            OsString::from("hi"),
        ]
    );
}

#[test]
fn spawn_spec_interactive_has_no_run_arg() {
    let adapter = OpencodeAdapter::new();
    let req = SpawnRequest::new("/tmp/wt");
    let spec = adapter.spawn_spec(&req).expect("spec");
    assert!(
        !spec.args.iter().any(|a| a == &OsString::from("run")),
        "interactive should not include `run`, got {:?}",
        spec.args
    );
    assert_eq!(spec.stdin_mode, StdinMode::Pty);
    // Interactive opencode takes the project path as a positional arg.
    assert_eq!(spec.args.first(), Some(&OsString::from("/tmp/wt")));
}

#[test]
fn spawn_spec_honours_program_override() {
    let mut req = SpawnRequest::new("/tmp/wt");
    req.program_override = Some(PathBuf::from("/custom/opencode"));
    let spec = OpencodeAdapter::new().spawn_spec(&req).expect("spec");
    assert_eq!(spec.program, PathBuf::from("/custom/opencode"));
}

#[test]
fn spawn_spec_appends_extra_args_after_adapter_args() {
    let adapter = OpencodeAdapter::new();
    let mut req = SpawnRequest::new("/tmp/wt");
    req.stdin_mode = StdinMode::NullSink;
    req.prompt = Some("hi".into());
    req.extra_args = vec![OsString::from("--share"), OsString::from("--thinking")];
    let spec = adapter.spawn_spec(&req).expect("spec");
    // Extra args land at the tail, after the adapter-built run args.
    let last_two = &spec.args[spec.args.len() - 2..];
    assert_eq!(
        last_two,
        &[OsString::from("--share"), OsString::from("--thinking")]
    );
}

#[test]
fn spawn_spec_sets_term_when_caller_did_not() {
    let adapter = OpencodeAdapter::new();
    let req = SpawnRequest::new("/tmp/wt");
    let spec = adapter.spawn_spec(&req).expect("spec");
    let term = spec.env.iter().find(|(k, _)| k == "TERM").map(|(_, v)| v);
    assert_eq!(term, Some(&OsString::from("xterm-256color")));
}

#[test]
fn spawn_spec_does_not_overwrite_caller_term() {
    let adapter = OpencodeAdapter::new();
    let mut req = SpawnRequest::new("/tmp/wt");
    req.env = vec![(OsString::from("TERM"), OsString::from("dumb"))];
    let spec = adapter.spawn_spec(&req).expect("spec");
    // Exactly one TERM, and it's the caller's value.
    let terms: Vec<&OsString> = spec
        .env
        .iter()
        .filter(|(k, _)| k == "TERM")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(terms, vec![&OsString::from("dumb")]);
}

#[test]
fn encode_user_input_appends_lf() {
    let adapter = OpencodeAdapter::new();
    let bytes = adapter.encode_user_input("hi");
    assert_eq!(&*bytes, b"hi\n");
}

#[test]
fn graceful_stop_sequence_shape() {
    let seq = OpencodeAdapter::new().graceful_stop();
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

/// `discover()` reads the canonical flat layout
/// (`sessions/*.json`) and surfaces each session's id + cwd hint.
#[tokio::test]
async fn discover_reads_flat_layout() {
    let _g = MOCK_ENV.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    fs::create_dir_all(&root).unwrap();

    let proj_a = tmp.path().join("proj-a");
    let proj_b = tmp.path().join("proj-b");
    fs::create_dir_all(&proj_a).unwrap();
    fs::create_dir_all(&proj_b).unwrap();
    let canon_a = fs::canonicalize(&proj_a).unwrap();

    let file_a = root.join("ses_aaaaaaaaaaaaaaaaaaaa.json");
    let file_b = root.join("ses_bbbbbbbbbbbbbbbbbbbb.json");
    fs::write(
        &file_a,
        format!(
            "{{\"id\":\"ses_aaaaaaaaaaaaaaaaaaaa\",\"cwd\":\"{}\",\"title\":\"alpha\"}}",
            proj_a.display()
        ),
    )
    .unwrap();
    fs::write(
        &file_b,
        format!(
            "{{\"id\":\"ses_bbbbbbbbbbbbbbbbbbbb\",\"cwd\":\"{}\"}}",
            proj_b.display()
        ),
    )
    .unwrap();

    // Tolerated noise: a non-JSON file should be silently skipped.
    fs::write(root.join("ses_broken.json"), b"not json at all\n").unwrap();
    // Non-session extensions are ignored.
    fs::write(root.join("README.md"), b"# notes").unwrap();

    std::env::set_var(SESSIONS_DIR_ENV, &root);
    let adapter = OpencodeAdapter::new();

    let all = adapter
        .discover(&DiscoverHints::default())
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);

    assert_eq!(all.len(), 2, "expected 2 sessions, got {all:?}");
    let ids: Vec<&str> = all.iter().map(|s| s.external_id.as_str()).collect();
    assert!(ids.contains(&"ses_aaaaaaaaaaaaaaaaaaaa"));
    assert!(ids.contains(&"ses_bbbbbbbbbbbbbbbbbbbb"));
    // Title hint propagates when present.
    let alpha = all
        .iter()
        .find(|s| s.external_id == "ses_aaaaaaaaaaaaaaaaaaaa")
        .unwrap();
    assert_eq!(alpha.title_hint.as_deref(), Some("alpha"));

    // Filtered by project root: only proj-a comes back.
    std::env::set_var(SESSIONS_DIR_ENV, &root);
    let hints = DiscoverHints {
        project_root: Some(canon_a.clone()),
        source_path_override: None,
    };
    let filtered = adapter.discover(&hints).await.expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].external_id, "ses_aaaaaaaaaaaaaaaaaaaa");
}

/// `discover()` also handles a nested envelope shape
/// (`{"meta": {"id": ..., "cwd": ...}}`) seen in some opencode releases.
#[tokio::test]
async fn discover_reads_nested_envelope() {
    let _g = MOCK_ENV.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let nested = root.join("global");
    fs::create_dir_all(&nested).unwrap();

    let proj = tmp.path().join("proj");
    fs::create_dir_all(&proj).unwrap();

    let file = nested.join("ses_cccccccccccccccccccc.json");
    fs::write(
        &file,
        format!(
            "{{\"meta\":{{\"id\":\"ses_cccccccccccccccccccc\",\"worktree\":\"{}\"}}}}",
            proj.display()
        ),
    )
    .unwrap();

    std::env::set_var(SESSIONS_DIR_ENV, &root);
    let out = OpencodeAdapter::new()
        .discover(&DiscoverHints::default())
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].external_id, "ses_cccccccccccccccccccc");
    assert_eq!(out[0].project_hint.as_deref(), Some(proj.as_path()));
}

#[tokio::test]
async fn discover_returns_empty_when_root_missing() {
    let _g = MOCK_ENV.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let absent = tmp.path().join("does-not-exist");
    std::env::set_var(SESSIONS_DIR_ENV, &absent);
    let out = OpencodeAdapter::new()
        .discover(&DiscoverHints::default())
        .await
        .expect("discover");
    std::env::remove_var(SESSIONS_DIR_ENV);
    assert!(out.is_empty());
}
