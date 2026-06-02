//! End-to-end smoke test: drive the real `opencode` CLI through `la-pty`
//! using `OpencodeAdapter::spawn_spec`.
//!
//! Gated by `LA_RUN_OPENCODE_E2E=1` because it needs:
//!   - `opencode` on PATH,
//!   - configured provider credentials (`opencode auth list` non-empty),
//!   - network access to the provider's API,
//!   - and burns real tokens.
//!
//! CI opts in by setting the env var on dedicated jobs. Locally:
//!
//! ```bash
//! LA_RUN_OPENCODE_E2E=1 cargo test -p la-adapter --test real_opencode -- --nocapture
//! ```
//!
//! Acceptance (WEK-25 / M2.2): "through la-pty start real opencode CLI,
//! send a prompt once → reply." That's exactly what this verifies —
//! adapter builds a `run --format json` spec, la-pty spawns it, we
//! drain the PTY until the child exits, and assert *some* response
//! landed on stdout.

use std::process::Command;
use std::time::Duration;

use la_adapter::opencode::OpencodeAdapter;
use la_adapter::{AgentAdapter, SpawnRequest, StdinMode};
use la_pty::{spawn, CommandBuilder, PtySize};
use tokio::time::timeout;

const E2E_ENV_FLAG: &str = "LA_RUN_OPENCODE_E2E";
const READ_TIMEOUT: Duration = Duration::from_secs(120);

fn e2e_enabled() -> bool {
    std::env::var(E2E_ENV_FLAG)
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn opencode_on_path() -> bool {
    Command::new("opencode")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_opencode_one_shot_prompt_reply() {
    if !e2e_enabled() {
        eprintln!("skipping: set {E2E_ENV_FLAG}=1 to run real-opencode E2E test");
        return;
    }
    if !opencode_on_path() {
        eprintln!("skipping: `opencode --version` failed (CLI missing or unauth)");
        return;
    }

    let adapter = OpencodeAdapter::new();
    let mut req = SpawnRequest::new(std::env::temp_dir());
    req.stdin_mode = StdinMode::NullSink;
    req.prompt = Some("Reply with exactly one word: PONG. No punctuation, no extra text.".into());

    let spec = adapter.spawn_spec(&req).expect("spawn_spec");

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
    let mut child = spawn(cmd, size).expect("la-pty spawn real opencode");

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
    eprintln!("real-opencode exit status: {status:?}");
    eprintln!(
        "real-opencode stdout (truncated):\n{}",
        &stdout.chars().take(2000).collect::<String>()
    );

    assert!(
        status.success(),
        "opencode run --format json should exit 0, got {status:?}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "expected a non-empty reply on the PTY stream"
    );
}
