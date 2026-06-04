//! WEK-71 / M4.2 — `lad config` subcommand acceptance tests.
//!
//! Drives the `lad` binary as a subprocess to cover:
//!
//! - `lad config check` exits non-zero on a `deny_unknown_fields` typo
//!   and lists the unknown field in stderr.
//! - `lad config check` accepts the committed example template.
//! - `lad config path` prints exactly the env-override path when
//!   `LAZYAGENTS_CONFIG` is set.
//! - `lad config show` annotates `--log-level` as "from CLI flag" (and
//!   without it, as "from env").
//! - Bootstrapping with `LAZYAGENTS_LOG` (deprecated alias) prints the
//!   deprecation warning to stderr.
//!
//! The binary is located via `LAD_BIN`, set the same way M1.7's
//! acceptance suite already requires:
//!
//! ```ignore
//! LAD_BIN=$(cargo build -p la-daemon --bin lad --message-format=json \
//!     | jq -r 'select(.reason=="compiler-artifact").executable | strings' | head -n1) \
//!     cargo test -p la-daemon --test wek71_config
//! ```
//!
//! When `LAD_BIN` is unset the test prints a skip line and passes —
//! same posture as the daemonize acceptance test, so PR CI does not
//! need a pre-built binary to be green.

use std::path::Path;
use std::process::Command;

fn lad_bin() -> Option<std::path::PathBuf> {
    std::env::var_os("LAD_BIN").map(std::path::PathBuf::from)
}

fn assert_skip(reason: &str) {
    eprintln!("{reason} — set LAD_BIN to enable this test");
}

#[test]
fn config_check_rejects_unknown_field_and_lists_section_hint() {
    let Some(lad) = lad_bin() else {
        assert_skip("LAD_BIN unset; skipping config_check_rejects_unknown_field");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("config.toml");
    std::fs::write(&cfg, "[daemon]\nsokcet_path = \"/tmp/x\"\n").unwrap();

    let out = run_lad(
        &lad,
        &["config", "check", "--config", cfg.to_str().unwrap()],
    );
    assert!(!out.status.success(), "stderr={}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("sokcet_path"),
        "expected stderr to name the unknown field; got: {stderr}"
    );
    assert!(
        stderr.contains("unknown field") || stderr.contains("schema"),
        "expected stderr to mention unknown field; got: {stderr}"
    );
    assert!(
        stderr.contains("accepted top-level sections"),
        "expected stderr to list known sections; got: {stderr}"
    );
}

#[test]
fn config_check_accepts_committed_example() {
    let Some(lad) = lad_bin() else {
        assert_skip("LAD_BIN unset; skipping config_check_accepts_committed_example");
        return;
    };
    // The example lives next to the daemon crate and is committed to
    // the repo — `lad config check` against it MUST pass so M4.0.5 CI
    // can use this exact file as the cross-platform smoke target.
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/config.example.toml");
    let out = run_lad(
        &lad,
        &["config", "check", "--config", example.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "config check on example failed: stdout={} stderr={}",
        out.stdout_str(),
        out.stderr_str()
    );
}

#[test]
fn config_path_prints_lazyagents_config_override() {
    let Some(lad) = lad_bin() else {
        assert_skip("LAD_BIN unset; skipping config_path_prints_override");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("explicit.toml");
    std::fs::write(&cfg, "").unwrap();
    let mut cmd = Command::new(&lad);
    cmd.args(["config", "path"]);
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("HOME");
    cmd.env("LAZYAGENTS_CONFIG", &cfg);
    let out = collect(cmd);
    assert!(out.status.success(), "stderr={}", out.stderr_str());
    let stdout = out.stdout_str();
    assert!(
        stdout.contains(cfg.to_str().unwrap()),
        "expected stdout to contain {}; got: {stdout}",
        cfg.display()
    );
}

#[test]
fn config_show_marks_cli_log_level_as_cli_source() {
    let Some(lad) = lad_bin() else {
        assert_skip("LAD_BIN unset; skipping config_show_marks_cli_source");
        return;
    };
    let out = run_lad(&lad, &["config", "show", "--log-level", "debug"]);
    assert!(out.status.success(), "stderr={}", out.stderr_str());
    let stdout = out.stdout_str();
    assert!(
        stdout.contains("log_level = \"debug\""),
        "expected resolved log_level in output: {stdout}"
    );
    assert!(
        stdout.contains("# from CLI flag"),
        "expected provenance comment 'from CLI flag': {stdout}"
    );
}

#[test]
fn lazyagents_log_deprecation_prints_warning_to_stderr() {
    let Some(lad) = lad_bin() else {
        assert_skip("LAD_BIN unset; skipping lazyagents_log_deprecation_warning");
        return;
    };
    // `lad config show` exercises the EnvSnapshot path that emits the
    // deprecation warning. Setting LAZYAGENTS_LOG (without LOG_LEVEL)
    // is the only way to trigger it.
    let mut cmd = Command::new(&lad);
    cmd.args(["config", "show"]);
    cmd.env_remove("LAZYAGENTS_LOG_LEVEL");
    cmd.env("LAZYAGENTS_LOG", "debug");
    let out = collect(cmd);
    assert!(out.status.success(), "stderr={}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("LAZYAGENTS_LOG is deprecated"),
        "expected deprecation warning; stderr={stderr}"
    );
    // And confirm the value was *applied* (env source picked it up).
    let stdout = out.stdout_str();
    assert!(
        stdout.contains("log_level = \"debug\"") && stdout.contains("# from env"),
        "expected env-sourced log_level=debug in show output; stdout={stdout}"
    );
}

#[test]
fn start_path_rejects_listen_tcp_config_value() {
    // Reviewer round 2 blocker: `lad start --config bad.toml` with
    // `listen_tcp = "..."` must NOT silently fall through — share the
    // same validation as `lad config check`. Driving `start` would
    // require a runtime, so exercise `metrics` instead, which also
    // goes through `derive_runtime()` and is fast-exit.
    let Some(lad) = lad_bin() else {
        assert_skip("LAD_BIN unset; skipping start_path_rejects_listen_tcp");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("bad.toml");
    std::fs::write(&cfg, "[daemon]\nlisten_tcp = \"0.0.0.0:7042\"\n").unwrap();
    let out = run_lad(&lad, &["metrics", "--config", cfg.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "expected non-zero exit; stderr={}",
        out.stderr_str()
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("listen_tcp") && stderr.contains("validation"),
        "expected validation error mentioning listen_tcp; stderr={stderr}"
    );
}

#[test]
fn cli_log_level_typo_is_rejected_not_silently_demoted() {
    let Some(lad) = lad_bin() else {
        assert_skip("LAD_BIN unset; skipping cli_log_level_typo_rejected");
        return;
    };
    let out = run_lad(&lad, &["config", "show", "--log-level", "typo"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit on bogus --log-level; stdout={} stderr={}",
        out.stdout_str(),
        out.stderr_str()
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("--log-level") && stderr.contains("typo"),
        "expected error to name --log-level and the offending value; stderr={stderr}"
    );
}

fn run_lad(lad: &Path, args: &[&str]) -> Out {
    let mut cmd = Command::new(lad);
    cmd.args(args);
    // Tests must NOT inherit a developer's $LAZYAGENTS_CONFIG.
    cmd.env_remove("LAZYAGENTS_CONFIG");
    cmd.env_remove("LAZYAGENTS_LOG");
    cmd.env_remove("LAZYAGENTS_LOG_LEVEL");
    collect(cmd)
}

fn collect(mut cmd: Command) -> Out {
    let output = cmd.output().expect("failed to spawn lad");
    Out {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

struct Out {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Out {
    fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }
    fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).to_string()
    }
}
