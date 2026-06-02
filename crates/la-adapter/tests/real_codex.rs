//! End-to-end smoke test: drive the real `codex` CLI through `la-pty`
//! using `CodexAdapter::spawn_spec`.
//!
//! Gated by `LA_RUN_CODEX_E2E=1` because it needs:
//!   - `codex` on PATH,
//!   - a logged-in account (`codex login status` succeeds),
//!   - network access to OpenAI's API,
//!   - and burns real tokens.
//!
//! CI opts in by setting the env var on dedicated jobs. Locally:
//!
//! ```bash
//! LA_RUN_CODEX_E2E=1 cargo test -p la-adapter --test real_codex -- --nocapture
//! ```
//!
//! Acceptance (WEK-24 / M2.1): "through la-pty start real codex CLI,
//! send a prompt once → reply." That's exactly what this verifies —
//! adapter builds an `exec --json` spec, la-pty spawns it, we drain
//! the PTY until the child exits, and assert *some* response landed on
//! stdout.

use std::process::Command;
use std::time::Duration;

use la_adapter::codex::CodexAdapter;
use la_adapter::{AgentAdapter, SpawnRequest, StdinMode};
use la_pty::{spawn, CommandBuilder, PtySize};
use tokio::time::timeout;

const E2E_ENV_FLAG: &str = "LA_RUN_CODEX_E2E";
const READ_TIMEOUT: Duration = Duration::from_secs(120);

fn e2e_enabled() -> bool {
    std::env::var(E2E_ENV_FLAG)
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn codex_on_path() -> bool {
    Command::new("codex")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_codex_one_shot_prompt_reply() {
    if !e2e_enabled() {
        eprintln!("skipping: set {E2E_ENV_FLAG}=1 to run real-codex E2E test");
        return;
    }
    if !codex_on_path() {
        eprintln!("skipping: `codex --version` failed (CLI missing or unauth)");
        return;
    }

    let adapter = CodexAdapter::new();
    let mut req = SpawnRequest::new(std::env::temp_dir());
    req.stdin_mode = StdinMode::NullSink;
    req.prompt = Some("Reply with exactly one word: PONG. No punctuation, no extra text.".into());

    let spec = adapter.spawn_spec(&req).expect("spawn_spec");

    // Wire SpawnSpec → CommandBuilder locally — the daemon (la-core)
    // owns this conversion in production; tests keep it inline.
    let mut cmd = CommandBuilder::new(spec.program.clone());
    cmd.cwd(spec.cwd.clone());
    for a in &spec.args {
        cmd.arg(a);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let size = PtySize {
        rows: spec.pty.rows,
        cols: spec.pty.cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut child = spawn(cmd, size).expect("la-pty spawn real codex");

    // Drain until EOF or timeout.
    let collected = timeout(READ_TIMEOUT, async {
        let mut buf = Vec::<u8>::new();
        while let Some(chunk) = child.reader.recv().await {
            buf.extend_from_slice(&chunk);
        }
        buf
    })
    .await
    .expect("PTY did not EOF in time");

    let status = child.wait().await.expect("wait");
    let stdout = String::from_utf8_lossy(&collected);
    eprintln!("real-codex exit status: {status:?}");
    eprintln!(
        "real-codex stdout (truncated):\n{}",
        &stdout.chars().take(2000).collect::<String>()
    );

    assert!(
        status.success(),
        "codex exec --json should exit 0, got {status:?}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "expected a non-empty reply on the PTY stream"
    );
}
