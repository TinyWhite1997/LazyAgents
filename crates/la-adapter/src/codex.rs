//! `codex` (OpenAI Codex CLI) adapter.
//!
//! M2.1 scope (per WEK-24): implement the full [`AgentAdapter`] surface
//! against the `codex` CLI shipped on PATH. Mirrors the structural
//! conventions of [`crate::claude`] — descriptor, version probe,
//! spawn-spec branching on [`StdinMode`], a line-buffered JSONL
//! `parse_chunk`, and an on-disk `discover` that walks
//! `~/.codex/sessions/`.
//!
//! Notes on the target CLI (validated against `codex-cli 0.135.0`):
//!
//! - `codex --version` prints `codex-cli 0.135.0\n` and exits 0.
//! - `codex login status` prints `Not logged in` (exit non-zero) when
//!   unauthenticated; we use it as a cheap auth probe after `--version`
//!   succeeds.
//! - There is no `codex sessions list --json` subcommand in 0.135.0
//!   (the architecture doc lists it as the preferred path, but the
//!   binary doesn't ship it). We fall back to the on-disk store —
//!   `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`, whose
//!   first JSONL line is a `session_meta` record carrying the session
//!   `id` and the originating `cwd`. Legacy flat `~/.codex/sessions/*.json`
//!   files are tolerated for older installs.
//! - Non-interactive one-shot: `codex exec --json [--cd <dir>] <prompt>`
//!   emits JSONL events on stdout. We prefer this when stdin is a
//!   [`StdinMode::NullSink`] with a prompt.
//! - The interactive TUI submits on a single newline (`\n`), unlike
//!   claude's `\r`.
//! - Resume (`codex resume <id>`) is intentionally NOT modelled in the
//!   spec — per the architecture doc, resume spawns a fresh process
//!   and never inherits a prior PTY. `discover()` surfaces the external
//!   id so the daemon can build a resume spawn elsewhere.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::{
    AdapterDescriptor, AdapterError, AdapterEvent, AgentAdapter, DiscoverHints, DiscoveredSession,
    ParserState, ProbeResult, SpawnRequest, SpawnSpec, StdinMode, StopAction, StopSequence,
    StopSignal,
};

const DEFAULT_PROGRAM: &str = "codex";
const DOCS_URL: &str = "https://developers.openai.com/codex/cli";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Env var that overrides where [`CodexAdapter::discover`] looks for
/// session rollouts. Defaults to `<home>/.codex/sessions`. Used by
/// tests so they don't pollute the real user's home directory.
pub const SESSIONS_DIR_ENV: &str = "CODEX_SESSIONS_DIR";

/// Adapter for OpenAI's `codex` CLI ("Codex CLI").
///
/// The adapter is stateless; one instance per registry entry is fine.
/// All configurable bits (alternative program path, extra args) come
/// through [`SpawnRequest`].
#[derive(Debug, Default, Clone)]
pub struct CodexAdapter {
    /// Optional override for the executable used by `probe`.
    /// `spawn_spec` honours `SpawnRequest::program_override` first, then
    /// this, then `DEFAULT_PROGRAM`.
    program: Option<PathBuf>,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an adapter that probes / defaults to a specific executable
    /// path. Useful in tests (to point at `mock-cli`) and in production
    /// when the user has set `adapters.codex.command` in their config.
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: Some(program.into()),
        }
    }

    fn resolved_program(&self, req_override: Option<&Path>) -> PathBuf {
        if let Some(p) = req_override {
            return p.to_path_buf();
        }
        if let Some(p) = &self.program {
            return p.clone();
        }
        PathBuf::from(DEFAULT_PROGRAM)
    }

    /// Best-effort secondary auth probe: run `<program> login status` and
    /// classify "Not logged in" stdout / non-zero exit as
    /// unauthenticated. Returns `None` on any spawn or timeout failure
    /// so the caller can fall through to its existing classification.
    async fn login_status_indicates_unauth(&self) -> Option<bool> {
        let program = self.resolved_program(None);
        let mut cmd = Command::new(&program);
        cmd.arg("login")
            .arg("status")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().ok()?;
        let output = timeout(PROBE_TIMEOUT, child.wait_with_output())
            .await
            .ok()?
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let unauth = looks_unauthenticated(&stdout, &stderr) || !output.status.success();
        Some(unauth)
    }
}

#[async_trait]
impl AgentAdapter for CodexAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "codex",
            display_name: "Codex CLI",
            default_program: DEFAULT_PROGRAM,
            docs_url: DOCS_URL,
        }
    }

    async fn probe(&self) -> ProbeResult {
        let program = self.resolved_program(None);

        let mut cmd = Command::new(&program);
        cmd.arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let spawn = cmd.spawn();
        let child = match spawn {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ProbeResult::NotInstalled {
                    hint: format!(
                        "`{}` not found on PATH; install Codex CLI or set adapters.codex.command",
                        program.display()
                    ),
                }
            }
            Err(e) => return ProbeResult::Error { detail: format!("spawn: {e}") },
        };

        let output = match timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return ProbeResult::Error {
                    detail: format!("wait: {e}"),
                }
            }
            Err(_) => {
                return ProbeResult::Error {
                    detail: format!(
                        "`{} --version` timed out after {:?}",
                        program.display(),
                        PROBE_TIMEOUT
                    ),
                }
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Some codex builds surface auth state on `--version`'s stderr;
        // tolerate it inline so the user sees a clean Unauthenticated.
        if looks_unauthenticated(&stdout, &stderr) {
            return ProbeResult::Unauthenticated {
                docs_url: DOCS_URL.to_string(),
            };
        }

        if !output.status.success() {
            return ProbeResult::Error {
                detail: format!(
                    "`{} --version` exited {:?}: stderr={}",
                    program.display(),
                    output.status.code(),
                    stderr.trim()
                ),
            };
        }

        let version = match parse_version(&stdout) {
            Some(v) => v,
            None => {
                return ProbeResult::Error {
                    detail: format!("unrecognized --version output: {:?}", stdout.trim()),
                }
            }
        };

        // `--version` is happy; secondary `login status` probe catches
        // installs where the CLI runs but no account is linked. Best
        // effort — if it fails to spawn we fall through to Available.
        if matches!(self.login_status_indicates_unauth().await, Some(true)) {
            return ProbeResult::Unauthenticated {
                docs_url: DOCS_URL.to_string(),
            };
        }

        ProbeResult::Available { version }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        let program = self.resolved_program(req.program_override.as_deref());

        let mut args: Vec<OsString> = Vec::new();

        // Non-interactive (cron / scripted) runs map to `codex exec
        // --json` so the child emits JSONL events and exits. We always
        // pass `--cd` so the daemon's chosen worktree wins over codex's
        // home-cwd defaulting. Interactive sessions stay in the default
        // TUI and we let the daemon write the first prompt via
        // encode_user_input() once the child is ready.
        match req.stdin_mode {
            StdinMode::NullSink => {
                if let Some(prompt) = req.prompt.as_deref() {
                    args.push(OsString::from("exec"));
                    args.push(OsString::from("--json"));
                    args.push(OsString::from("--cd"));
                    args.push(OsString::from(req.cwd.as_os_str()));
                    args.push(OsString::from(prompt));
                }
            }
            StdinMode::Pty => {
                // Interactive: only thread `--cd` through when the
                // caller actually specified one (non-empty). The TUI
                // accepts no positional prompt.
                if !req.cwd.as_os_str().is_empty() {
                    args.push(OsString::from("--cd"));
                    args.push(OsString::from(req.cwd.as_os_str()));
                }
            }
        }

        args.extend(req.extra_args.iter().cloned());

        // Hint the CLI which terminal capabilities to assume — same
        // policy as the claude adapter. Caller-supplied env wins.
        let mut env: Vec<(OsString, OsString)> = Vec::new();
        if !req.env.iter().any(|(k, _)| k == "TERM") {
            env.push((OsString::from("TERM"), OsString::from("xterm-256color")));
        }
        env.extend(req.env.iter().cloned());

        Ok(SpawnSpec {
            program,
            args,
            env,
            cwd: req.cwd.clone(),
            pty: req.pty,
            stdin_mode: req.stdin_mode,
        })
    }

    fn encode_user_input(&self, text: &str) -> Bytes {
        // Codex's TUI submits on a single LF (unlike claude's CR).
        // Internal newlines in the user's text are passed through
        // verbatim; we only append one LF at the end so multi-line
        // pastes don't fire prematurely.
        let mut out = Vec::with_capacity(text.len() + 1);
        out.extend_from_slice(text.as_bytes());
        out.push(b'\n');
        Bytes::from(out)
    }

    fn graceful_stop(&self) -> StopSequence {
        // Codex does not expose a stable in-band exit command across
        // versions (the TUI's `/quit` is present in 0.135.0 but the
        // architecture doc cautions against relying on it). Default to
        // a signal-only sequence so we never wedge on a missing
        // command; the daemon's OS layer maps these to SIGTERM/SIGKILL
        // (or CTRL_BREAK / TerminateProcess on Windows).
        StopSequence(vec![
            StopAction::Signal(StopSignal::Terminate),
            StopAction::AwaitExit(Duration::from_secs(2)),
            StopAction::Signal(StopSignal::Kill),
        ])
    }

    async fn discover(
        &self,
        hints: &DiscoverHints,
    ) -> Result<Vec<DiscoveredSession>, AdapterError> {
        let root = sessions_root();
        let root = match root {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let mut out: Vec<DiscoveredSession> = Vec::new();
        let want_root = hints
            .project_root
            .as_deref()
            .map(canonicalize_or_keep);

        if !root.exists() {
            return Ok(out);
        }

        // Walk both the nested YYYY/MM/DD layout (current) and any
        // flat `*.json` files left over from older codex installs.
        let entries = match collect_session_files(&root) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(AdapterError::Transient(format!(
                    "cannot read codex sessions dir {}: {e}",
                    root.display()
                )));
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %root.display(), "discover: walk failed");
                return Ok(out);
            }
        };

        for file in entries {
            let Some(meta) = read_session_meta(&file) else {
                continue;
            };
            if let Some(ref want) = want_root {
                let got = canonicalize_or_keep(Path::new(&meta.cwd));
                if &got != want {
                    continue;
                }
            }
            out.push(DiscoveredSession {
                external_id: meta.id,
                project_hint: Some(PathBuf::from(meta.cwd)),
                title_hint: None,
            });
        }

        Ok(out)
    }

    fn parse_chunk(&self, chunk: &[u8], st: &mut ParserState) -> Vec<AdapterEvent> {
        // Line-buffered JSONL parser. M2.1 acceptance is "doesn't crash
        // on real `codex --json`" — every complete line is round-tripped
        // as a `Passthrough` event. We do a `serde_json` sanity-parse so
        // a future milestone can turn the `Ok` branch into a structured
        // event; for now both branches just re-emit the line.
        if chunk.is_empty() && st.partial.is_empty() {
            return Vec::new();
        }
        st.partial.extend_from_slice(chunk);

        let mut events: Vec<AdapterEvent> = Vec::new();
        while let Some(nl) = st.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = st.partial.drain(..=nl).collect();
            // Drop the trailing newline before validating, but emit the
            // full line (including the newline) so downstream renderers
            // see the framing the backend produced.
            let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
            let _ = serde_json::from_slice::<serde_json::Value>(trimmed);
            events.push(AdapterEvent::Passthrough(Bytes::copy_from_slice(&line)));
        }
        events
    }
}

/// Minimal shape of the first JSONL line in a codex session rollout.
/// Only the fields [`CodexAdapter::discover`] needs are pulled out.
#[derive(Debug, Deserialize)]
struct SessionMetaWire {
    #[serde(rename = "type")]
    kind: String,
    payload: SessionMetaPayload,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: String,
}

/// Pull the first sem-ver-ish token out of `codex --version` output.
///
/// Expected shapes:
///
/// - `"codex-cli 0.135.0\n"`
/// - `"codex 0.135.0\n"`
/// - `"v0.135.0\n"`
///
/// We accept any whitespace-separated token of the form `N(.N)+`
/// optionally followed by `-suffix` (pre-release tags).
fn parse_version(s: &str) -> Option<String> {
    s.split_whitespace().find_map(|raw| {
        let tok = raw.strip_prefix(['v', 'V']).unwrap_or(raw);
        let core = tok.split('-').next().unwrap_or(tok);
        let mut parts = core.split('.');
        let first = parts.next()?;
        if first.is_empty() || !first.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let mut count = 1;
        for p in parts {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            count += 1;
        }
        if count >= 2 {
            Some(tok.to_string())
        } else {
            None
        }
    })
}

fn looks_unauthenticated(stdout: &str, stderr: &str) -> bool {
    let needles = [
        "not logged in",
        "not authenticated",
        "unauthenticated",
        "please log in",
        "please login",
        "run `codex login`",
        "run \"codex login\"",
        "codex login",
        "auth required",
    ];
    let hay_stdout = stdout.to_lowercase();
    let hay_stderr = stderr.to_lowercase();
    needles
        .iter()
        .any(|n| hay_stdout.contains(n) || hay_stderr.contains(n))
}

/// Resolve the on-disk sessions root. Honours `CODEX_SESSIONS_DIR`
/// first (test override), then `<HOME>/.codex/sessions`. Returns `None`
/// when neither is available — discover() treats that as "no sessions".
fn sessions_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(SESSIONS_DIR_ENV) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Walk the sessions root and return paths of every plausible rollout
/// file: nested `*.jsonl` under `YYYY/MM/DD/` and legacy flat `*.json`
/// directly under the root.
fn collect_session_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in rd.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                if ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("json") {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

/// Read the first line of a rollout file and try to deserialize the
/// `session_meta` record. Returns `None` for any I/O or shape error —
/// callers skip malformed entries silently.
fn read_session_meta(path: &Path) -> Option<SessionMetaPayload> {
    let bytes = std::fs::read(path).ok()?;
    let first = bytes.split(|b| *b == b'\n').next().unwrap_or(&[]);
    if first.is_empty() {
        return None;
    }
    match serde_json::from_slice::<SessionMetaWire>(first) {
        Ok(w) if w.kind == "session_meta" => Some(w.payload),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "discover: skipping malformed session_meta");
            None
        }
    }
}

fn canonicalize_or_keep(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_stable() {
        let d = CodexAdapter::new().descriptor();
        assert_eq!(d.id, "codex");
        assert_eq!(d.default_program, "codex");
        assert_eq!(d.display_name, "Codex CLI");
        assert_eq!(d.docs_url, "https://developers.openai.com/codex/cli");
    }

    #[test]
    fn parses_real_world_version_shapes() {
        assert_eq!(
            parse_version("codex-cli 0.135.0\n").as_deref(),
            Some("0.135.0")
        );
        assert_eq!(parse_version("codex 0.135.0\n").as_deref(), Some("0.135.0"));
        assert_eq!(parse_version("v0.135.0\n").as_deref(), Some("0.135.0"));
        assert_eq!(
            parse_version("codex-cli 0.135.0-beta.1\n").as_deref(),
            Some("0.135.0-beta.1")
        );
        assert_eq!(parse_version("nothing here\n"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn encode_user_input_appends_lf() {
        let bytes = CodexAdapter::new().encode_user_input("hello");
        assert_eq!(bytes.as_ref(), b"hello\n");
    }

    #[test]
    fn encode_user_input_passes_internal_newlines() {
        let bytes = CodexAdapter::new().encode_user_input("line1\nline2");
        assert_eq!(bytes.as_ref(), b"line1\nline2\n");
    }

    #[test]
    fn graceful_stop_is_signal_only_and_ends_in_kill() {
        let seq = CodexAdapter::new().graceful_stop();
        // No SendInput phase by default — see doc comment for rationale.
        assert!(!seq.0.iter().any(|s| matches!(s, StopAction::SendInput(_))));
        match seq.0.first().unwrap() {
            StopAction::Signal(StopSignal::Terminate) => {}
            other => panic!("expected Terminate first, got {other:?}"),
        }
        match seq.0.last().unwrap() {
            StopAction::Signal(StopSignal::Kill) => {}
            other => panic!("expected final Kill, got {other:?}"),
        }
    }

    #[test]
    fn unauthenticated_detector() {
        assert!(looks_unauthenticated(
            "Not logged in\n",
            ""
        ));
        assert!(looks_unauthenticated(
            "",
            "Error: not logged in. Please run codex login."
        ));
        assert!(!looks_unauthenticated("codex-cli 0.135.0", ""));
    }

    #[test]
    fn parse_chunk_emits_one_event_per_line() {
        let adapter = CodexAdapter::new();
        let mut st = ParserState::default();
        let events = adapter.parse_chunk(
            b"{\"type\":\"task_started\"}\n{\"type\":\"task_completed\"}\n",
            &mut st,
        );
        assert_eq!(events.len(), 2);
        assert!(st.partial.is_empty());
    }

    #[test]
    fn parse_chunk_buffers_partial_line() {
        let adapter = CodexAdapter::new();
        let mut st = ParserState::default();
        let first = adapter.parse_chunk(b"{\"type\":\"task_", &mut st);
        assert!(first.is_empty());
        let rest = adapter.parse_chunk(b"started\"}\n", &mut st);
        assert_eq!(rest.len(), 1);
    }

    #[test]
    fn parse_chunk_does_not_panic_on_garbage() {
        let adapter = CodexAdapter::new();
        let mut st = ParserState::default();
        let events = adapter.parse_chunk(b"this is not json\n", &mut st);
        assert_eq!(events.len(), 1);
    }
}
