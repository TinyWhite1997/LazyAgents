//! `opencode` (sst.dev OpenCode CLI) adapter.
//!
//! M2.2 scope (per WEK-25): implement the full [`AgentAdapter`] surface
//! against the `opencode` CLI shipped on PATH. Mirrors the structural
//! conventions of [`crate::codex`] — descriptor, version probe with a
//! secondary auth check, spawn-spec branching on [`StdinMode`], a
//! line-buffered JSONL `parse_chunk`, and an on-disk `discover` that
//! walks `$XDG_DATA_HOME/opencode/sessions/`.
//!
//! Notes on the target CLI (validated against `opencode 1.2.15`):
//!
//! - `opencode --version` prints `1.2.15\n` and exits 0.
//! - `opencode auth list` prints the configured providers (table form by
//!   default); when no credentials exist it prints a `0 credentials`
//!   footer. We use it as the secondary auth probe after `--version`
//!   succeeds. Like codex, we only flip to `Unauthenticated` on an
//!   explicit "no credentials" keyword — a non-zero exit without that
//!   keyword stays `Available` so older / re-named subcommands don't
//!   misreport as unauth.
//! - Non-interactive one-shot: `opencode run --format json [--dir <dir>]
//!   <message>` emits JSONL events on stdout. We prefer this when stdin
//!   is a [`StdinMode::NullSink`] with a prompt.
//! - The interactive TUI submits on a single newline (`\n`), like codex.
//! - Resume (`opencode run --session <id>`) is intentionally NOT modelled
//!   in the spec — per the architecture doc, resume spawns a fresh
//!   process and never inherits a prior PTY. `discover()` surfaces the
//!   external id so the daemon can build a resume spawn elsewhere.
//! - Session store: `$XDG_DATA_HOME/opencode/sessions/*.json` (one JSON
//!   document per session). We honour `OPENCODE_SESSIONS_DIR` first
//!   (test override), then `$XDG_DATA_HOME/opencode/sessions`, then
//!   `<HOME>/.local/share/opencode/sessions`. We also tolerate the
//!   `storage/session/` layout used by some releases as a fallback.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::ext_time::file_mtime_rfc3339;
use crate::{
    AdapterDescriptor, AdapterError, AdapterEvent, AgentAdapter, DiscoverHints, DiscoveredSession,
    ParserState, ProbeResult, SpawnRequest, SpawnSpec, StdinMode, StopAction, StopSequence,
    StopSignal,
};

const DEFAULT_PROGRAM: &str = "opencode";
const DOCS_URL: &str = "https://opencode.ai/docs/";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Env var that overrides where [`OpencodeAdapter::discover`] looks for
/// session JSON files. Defaults to `$XDG_DATA_HOME/opencode/sessions`
/// (or `<home>/.local/share/opencode/sessions`). Used by tests so they
/// don't pollute the real user's data dir.
pub const SESSIONS_DIR_ENV: &str = "OPENCODE_SESSIONS_DIR";

/// Adapter for sst.dev's `opencode` CLI ("OpenCode").
///
/// The adapter is stateless; one instance per registry entry is fine.
/// All configurable bits (alternative program path, extra args) come
/// through [`SpawnRequest`].
#[derive(Debug, Default, Clone)]
pub struct OpencodeAdapter {
    /// Optional override for the executable used by `probe`.
    /// `spawn_spec` honours `SpawnRequest::program_override` first, then
    /// this, then `DEFAULT_PROGRAM`.
    program: Option<PathBuf>,
}

impl OpencodeAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an adapter that probes / defaults to a specific executable
    /// path. Useful in tests (to point at `mock-cli`) and in production
    /// when the user has set `adapters.opencode.command` in their config.
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

    /// Best-effort secondary auth probe: run `<program> auth list` and
    /// classify the output. Returns `Some(true)` ONLY when stdout/stderr
    /// contains an explicit "no credentials" keyword. A non-zero exit
    /// without such a keyword is treated as `Some(false)` — older /
    /// newer opencode builds may rename or remove the `auth list`
    /// subcommand, and we don't want to misreport "subcommand missing"
    /// as "unauthenticated". Returns `None` on spawn / timeout failure
    /// so the caller falls through to its existing classification
    /// (typically `Available`).
    async fn auth_list_indicates_unauth(&self) -> Option<bool> {
        let program = self.resolved_program(None);
        let mut cmd = Command::new(&program);
        cmd.arg("auth")
            .arg("list")
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
        Some(looks_unauthenticated(&stdout, &stderr))
    }
}

#[async_trait]
impl AgentAdapter for OpencodeAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "opencode",
            display_name: "OpenCode",
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
                        "`{}` not found on PATH; install OpenCode or set adapters.opencode.command",
                        program.display()
                    ),
                }
            }
            Err(e) => {
                return ProbeResult::Error {
                    detail: format!("spawn: {e}"),
                }
            }
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

        // Some opencode builds surface auth state on `--version`'s
        // stderr; tolerate it inline so the user sees a clean
        // Unauthenticated.
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

        // `--version` is happy; secondary `auth list` probe catches
        // installs where the CLI runs but no provider credentials are
        // configured. Best effort — if it fails to spawn we fall
        // through to Available.
        if matches!(self.auth_list_indicates_unauth().await, Some(true)) {
            return ProbeResult::Unauthenticated {
                docs_url: DOCS_URL.to_string(),
            };
        }

        ProbeResult::Available { version }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        let program = self.resolved_program(req.program_override.as_deref());

        let mut args: Vec<OsString> = Vec::new();

        // Non-interactive (cron / scripted) runs map to `opencode run
        // --format json` so the child emits JSONL events and exits. We
        // always pass `--dir` so the daemon's chosen worktree wins over
        // opencode's cwd defaulting. Interactive sessions stay in the
        // default TUI and we let the daemon write the first prompt via
        // encode_user_input() once the child is ready.
        match req.stdin_mode {
            StdinMode::NullSink => {
                if let Some(prompt) = req.prompt.as_deref() {
                    args.push(OsString::from("run"));
                    args.push(OsString::from("--format"));
                    args.push(OsString::from("json"));
                    if !req.cwd.as_os_str().is_empty() {
                        args.push(OsString::from("--dir"));
                        args.push(OsString::from(req.cwd.as_os_str()));
                    }
                    args.push(OsString::from(prompt));
                }
            }
            StdinMode::Pty => {
                // Interactive: TUI accepts a positional `project` path.
                // Only thread it through when the caller actually
                // specified one (non-empty).
                if !req.cwd.as_os_str().is_empty() {
                    args.push(OsString::from(req.cwd.as_os_str()));
                }
            }
        }

        args.extend(req.extra_args.iter().cloned());

        // Hint the CLI which terminal capabilities to assume — same
        // policy as the claude / codex adapters. Caller-supplied env
        // wins.
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
        // Opencode's TUI submits on a single LF. Internal newlines in
        // the user's text are passed through verbatim; we only append
        // one LF at the end so multi-line pastes don't fire
        // prematurely.
        let mut out = Vec::with_capacity(text.len() + 1);
        out.extend_from_slice(text.as_bytes());
        out.push(b'\n');
        Bytes::from(out)
    }

    fn graceful_stop(&self) -> StopSequence {
        // Opencode does not expose a stable in-band exit command across
        // versions; default to a signal-only sequence so we never wedge
        // on a missing command. The daemon's OS layer maps these to
        // SIGTERM/SIGKILL (or CTRL_BREAK / TerminateProcess on
        // Windows).
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
        let root = match hints.source_path_override.clone().or_else(sessions_root) {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let mut out: Vec<DiscoveredSession> = Vec::new();
        let want_root = hints.project_root.as_deref().map(canonicalize_or_keep);

        if !root.exists() {
            return Ok(out);
        }

        // Walk both the canonical flat `sessions/*.json` layout and the
        // newer nested `sessions/<scope>/<id>.json` layout some releases
        // use.
        let entries = match collect_session_files(&root) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(AdapterError::Transient(format!(
                    "cannot read opencode sessions dir {}: {e}",
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
                let Some(ref cwd) = meta.cwd else {
                    // Without a cwd hint we cannot match the filter.
                    continue;
                };
                let got = canonicalize_or_keep(Path::new(cwd));
                if &got != want {
                    continue;
                }
            }
            let created_at = meta
                .created_at
                .clone()
                .or_else(|| file_mtime_rfc3339(&file));
            out.push(DiscoveredSession {
                external_id: meta.id,
                project_hint: meta.cwd.map(PathBuf::from),
                title_hint: meta.title,
                external_path: Some(file.clone()),
                created_at,
            });
        }

        Ok(out)
    }

    fn parse_chunk(&self, chunk: &[u8], st: &mut ParserState) -> Vec<AdapterEvent> {
        // Line-buffered JSONL parser. M2.2 acceptance is "doesn't crash
        // on real `opencode run --format json`" — every complete line is
        // round-tripped as a `Passthrough` event. We do a `serde_json`
        // sanity-parse so a future milestone can turn the `Ok` branch
        // into a structured event; for now both branches just re-emit
        // the line.
        if chunk.is_empty() && st.partial.is_empty() {
            return Vec::new();
        }
        st.partial.extend_from_slice(chunk);

        let mut events: Vec<AdapterEvent> = Vec::new();
        while let Some(nl) = st.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = st.partial.drain(..=nl).collect();
            let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
            let _ = serde_json::from_slice::<serde_json::Value>(trimmed);
            events.push(AdapterEvent::Passthrough(Bytes::copy_from_slice(&line)));
        }
        events
    }
}

/// Minimal shape of an opencode session JSON file. Only the fields
/// [`OpencodeAdapter::discover`] needs are pulled out, and every field
/// other than `id` is optional so a wide range of historical layouts
/// parse cleanly.
#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: String,
    #[serde(default, alias = "cwd", alias = "worktree", alias = "directory")]
    cwd: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// Opencode persists the session's wall-clock start in one of
    /// several keys depending on release; the adapter falls back to the
    /// file's mtime when none match.
    #[serde(
        default,
        alias = "created_at",
        alias = "createdAt",
        alias = "started_at",
        alias = "startedAt",
        alias = "timestamp"
    )]
    created_at: Option<String>,
}

/// Pull the first sem-ver-ish token out of `opencode --version` output.
///
/// Expected shapes:
///
/// - `"1.2.15\n"`
/// - `"opencode 1.2.15\n"`
/// - `"v1.2.15\n"`
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
        "no credentials",
        "0 credentials",
        "please log in",
        "please login",
        "run `opencode auth login`",
        "run \"opencode auth login\"",
        "opencode auth login",
        "auth required",
    ];
    let hay_stdout = stdout.to_lowercase();
    let hay_stderr = stderr.to_lowercase();
    needles
        .iter()
        .any(|n| hay_stdout.contains(n) || hay_stderr.contains(n))
}

/// Resolve the on-disk sessions root. Honours `OPENCODE_SESSIONS_DIR`
/// first (test override), then `$XDG_DATA_HOME/opencode/sessions`, then
/// `<HOME>/.local/share/opencode/sessions`. Returns `None` when no
/// candidate is available — discover() treats that as "no sessions".
fn sessions_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(SESSIONS_DIR_ENV) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("opencode").join("sessions"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("opencode")
            .join("sessions"),
    )
}

/// Walk the sessions root and return paths of every plausible session
/// file: `*.json` (canonical) and `*.jsonl` (defensive, in case a future
/// release switches framing). Tolerates nested layouts so newer
/// `sessions/<scope>/<id>.json` paths are also picked up.
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
                if ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonl") {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

/// Read a session file and try to deserialize the metadata payload.
/// Tolerates two on-disk shapes:
///
/// - a bare JSON object (`{"id": "...", "cwd": "..."}`)
/// - a JSON object under a top-level `meta` / `session` / `payload` key
///   (some opencode releases nest the metadata)
///
/// Returns `None` for any I/O or shape error — callers skip malformed
/// entries silently. JSONL files are handled by parsing the first line
/// only, mirroring the codex adapter's behaviour.
fn read_session_meta(path: &Path) -> Option<SessionMetaPayload> {
    let bytes = std::fs::read(path).ok()?;

    // For JSONL we look at the first non-empty line; for JSON we feed
    // the whole document. We try the most direct shape first and only
    // fall back to nested envelopes when needed so callers don't pay
    // for extra parses on the hot path.
    let is_jsonl = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| e.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false);

    let slice: &[u8] = if is_jsonl {
        let first = bytes
            .split(|b| *b == b'\n')
            .find(|line| !line.is_empty())
            .unwrap_or(&[]);
        if first.is_empty() {
            return None;
        }
        let len = first.len();
        &bytes[..len]
    } else {
        &bytes[..]
    };

    if let Ok(direct) = serde_json::from_slice::<SessionMetaPayload>(slice) {
        return Some(direct);
    }

    // Nested envelope fallback.
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        meta: Option<SessionMetaPayload>,
        #[serde(default)]
        session: Option<SessionMetaPayload>,
        #[serde(default)]
        payload: Option<SessionMetaPayload>,
    }
    match serde_json::from_slice::<Envelope>(slice) {
        Ok(env) => env.meta.or(env.session).or(env.payload),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "discover: skipping malformed session file");
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
        let d = OpencodeAdapter::new().descriptor();
        assert_eq!(d.id, "opencode");
        assert_eq!(d.default_program, "opencode");
        assert_eq!(d.display_name, "OpenCode");
        assert_eq!(d.docs_url, "https://opencode.ai/docs/");
    }

    #[test]
    fn parses_real_world_version_shapes() {
        assert_eq!(parse_version("1.2.15\n").as_deref(), Some("1.2.15"));
        assert_eq!(
            parse_version("opencode 1.2.15\n").as_deref(),
            Some("1.2.15")
        );
        assert_eq!(parse_version("v1.2.15\n").as_deref(), Some("1.2.15"));
        assert_eq!(
            parse_version("opencode 1.2.15-beta.1\n").as_deref(),
            Some("1.2.15-beta.1")
        );
        assert_eq!(parse_version("nothing here\n"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn encode_user_input_appends_lf() {
        let bytes = OpencodeAdapter::new().encode_user_input("hello");
        assert_eq!(bytes.as_ref(), b"hello\n");
    }

    #[test]
    fn encode_user_input_passes_internal_newlines() {
        let bytes = OpencodeAdapter::new().encode_user_input("line1\nline2");
        assert_eq!(bytes.as_ref(), b"line1\nline2\n");
    }

    #[test]
    fn graceful_stop_is_signal_only_and_ends_in_kill() {
        let seq = OpencodeAdapter::new().graceful_stop();
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
        assert!(looks_unauthenticated("Not logged in\n", ""));
        assert!(looks_unauthenticated("", "0 credentials\n"));
        assert!(looks_unauthenticated(
            "",
            "Error: please run `opencode auth login`."
        ));
        assert!(!looks_unauthenticated("opencode 1.2.15", ""));
    }

    #[test]
    fn parse_chunk_emits_one_event_per_line() {
        let adapter = OpencodeAdapter::new();
        let mut st = ParserState::default();
        let events = adapter.parse_chunk(b"{\"type\":\"start\"}\n{\"type\":\"done\"}\n", &mut st);
        assert_eq!(events.len(), 2);
        assert!(st.partial.is_empty());
    }

    #[test]
    fn parse_chunk_buffers_partial_line() {
        let adapter = OpencodeAdapter::new();
        let mut st = ParserState::default();
        let first = adapter.parse_chunk(b"{\"type\":\"sta", &mut st);
        assert!(first.is_empty());
        let rest = adapter.parse_chunk(b"rt\"}\n", &mut st);
        assert_eq!(rest.len(), 1);
    }

    #[test]
    fn parse_chunk_does_not_panic_on_garbage() {
        let adapter = OpencodeAdapter::new();
        let mut st = ParserState::default();
        let events = adapter.parse_chunk(b"this is not json\n", &mut st);
        assert_eq!(events.len(), 1);
    }
}
