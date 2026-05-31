//! `la-adapter` integration tests against `mock-cli`.
//!
//! Covers the four pieces M0 / WEK-13 calls out:
//! probe ✓ (incl. `Unauthenticated` classification), spawn_spec ✓,
//! encode_user_input ✓, graceful_stop ✓ — and verifies the adapter
//! stays IPC- / SQLite-free (this whole file links `la-adapter` alone).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;

use la_adapter::claude::ClaudeAdapter;
use la_adapter::{
    AgentAdapter, ProbeResult, SpawnRequest, StdinMode, StopAction, StopSignal,
};

fn mock_cli() -> PathBuf {
    // Set by Cargo for bins in the same package.
    PathBuf::from(env!("CARGO_BIN_EXE_mock-cli"))
}

/// All probe-against-mock-cli tests share this mutex: they mutate the
/// process-wide `MOCK_CLI_MODE` env var and would race under cargo
/// test's default thread-per-test parallelism.
static MOCK_ENV: Mutex<()> = Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    MOCK_ENV.lock().unwrap_or_else(|p| p.into_inner())
}

#[tokio::test]
async fn probe_against_mock_cli_returns_available() {
    let _g = lock_env();
    std::env::remove_var("MOCK_CLI_MODE");
    let adapter = ClaudeAdapter::with_program(mock_cli());
    match adapter.probe().await {
        ProbeResult::Available { version } => {
            assert!(
                version.starts_with("2.1."),
                "expected 2.1.x style version, got {version:?}"
            );
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_missing_binary_as_not_installed() {
    let bogus = PathBuf::from("/this/path/should/not/exist/claude-xyz-9999");
    let adapter = ClaudeAdapter::with_program(bogus);
    match adapter.probe().await {
        ProbeResult::NotInstalled { hint } => {
            assert!(hint.contains("claude-xyz-9999"), "hint: {hint}");
        }
        other => panic!("expected NotInstalled, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_unauthenticated_via_stderr() {
    let _g = lock_env();
    std::env::set_var("MOCK_CLI_MODE", "unauth");
    let adapter = ClaudeAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    std::env::remove_var("MOCK_CLI_MODE");

    match result {
        ProbeResult::Unauthenticated { docs_url } => {
            assert!(docs_url.starts_with("https://"), "docs_url: {docs_url}");
        }
        other => panic!("expected Unauthenticated, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_garbage_output_as_error() {
    let _g = lock_env();
    std::env::set_var("MOCK_CLI_MODE", "garbage");
    let adapter = ClaudeAdapter::with_program(mock_cli());
    let result = adapter.probe().await;
    std::env::remove_var("MOCK_CLI_MODE");

    match result {
        ProbeResult::Error { detail } => {
            assert!(detail.contains("unrecognized"), "detail: {detail}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn spawn_spec_uses_mock_cli_program_when_overridden_per_request() {
    let adapter = ClaudeAdapter::new();
    let mut req = SpawnRequest::new(std::env::temp_dir());
    req.program_override = Some(mock_cli());
    let spec = adapter.spawn_spec(&req).expect("spec");
    assert_eq!(spec.program, mock_cli());
    assert_eq!(spec.stdin_mode, StdinMode::Pty);
}

#[test]
fn spawn_spec_nullsink_print_mode_carries_prompt() {
    let adapter = ClaudeAdapter::new();
    let mut req = SpawnRequest::new(std::env::temp_dir());
    req.stdin_mode = StdinMode::NullSink;
    req.prompt = Some("hello".into());
    let spec = adapter.spawn_spec(&req).expect("spec");
    assert!(spec.args.contains(&OsString::from("--print")));
    assert!(spec.args.contains(&OsString::from("hello")));
}

#[test]
fn encode_user_input_round_trip() {
    let adapter = ClaudeAdapter::new();
    let bytes = adapter.encode_user_input("/help");
    assert_eq!(&*bytes, b"/help\r");
}

#[test]
fn graceful_stop_sequence_shape() {
    let seq = ClaudeAdapter::new().graceful_stop();
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
    assert_eq!(kinds, vec!["send", "await", "term", "await", "kill"]);
}
