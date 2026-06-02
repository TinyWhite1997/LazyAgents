//! `claude` (Claude Code) adapter.
//!
//! M0 scope (per WEK-13): implement `probe`, `spawn_spec`,
//! `encode_user_input`, `graceful_stop`. `discover` and `parse_chunk`
//! use the trait defaults (structured output is deferred to M2).

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
    AdapterDescriptor, AdapterError, AgentAdapter, DiscoverHints, DiscoveredSession, ProbeResult,
    SpawnRequest, SpawnSpec, StdinMode, StopAction, StopSequence, StopSignal,
};

const DEFAULT_PROGRAM: &str = "claude";
const DOCS_URL: &str = "https://docs.claude.com/en/docs/claude-code";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Env override for the on-disk session store walked by
/// [`ClaudeAdapter::discover`]. When unset the adapter falls back to
/// `<HOME>/.claude/projects`.
pub const SESSIONS_DIR_ENV: &str = "CLAUDE_SESSIONS_DIR";

/// Adapter for Anthropic's `claude` CLI ("Claude Code").
///
/// The adapter is stateless; one instance per registry entry is fine.
/// All configurable bits (alternative program path, extra args) come
/// through [`SpawnRequest`].
#[derive(Debug, Default, Clone)]
pub struct ClaudeAdapter {
    /// Optional override for the executable used by `probe`.
    /// `spawn_spec` honours `SpawnRequest::program_override` first, then
    /// this, then `DEFAULT_PROGRAM`.
    program: Option<PathBuf>,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an adapter that probes / defaults to a specific executable
    /// path. Useful in tests (to point at `mock-cli`) and in production
    /// when the user has set `adapters.claude.command` in their config.
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
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "claude",
            display_name: "Claude Code",
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
                        "`{}` not found on PATH; install Claude Code or set adapters.claude.command",
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

        // The CLI surfaces auth state via the exit code + a stderr
        // keyword on some versions; tolerate both.
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

        match parse_version(&stdout) {
            Some(version) => ProbeResult::Available { version },
            None => ProbeResult::Error {
                detail: format!("unrecognized --version output: {:?}", stdout.trim()),
            },
        }
    }

    fn spawn_spec(&self, req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        let program = self.resolved_program(req.program_override.as_deref());

        let mut args: Vec<OsString> = Vec::new();

        // Non-interactive (cron / scripted) runs map to claude's `--print`
        // mode so the child prints the response and exits. Interactive
        // sessions stay in the default TUI and we let the daemon write
        // the first prompt through encode_user_input() once the child
        // is ready.
        if let StdinMode::NullSink = req.stdin_mode {
            if let Some(prompt) = req.prompt.as_deref() {
                args.push(OsString::from("--print"));
                args.push(OsString::from(prompt));
            }
        }

        args.extend(req.extra_args.iter().cloned());

        // Hint the CLI which terminal capabilities to assume — the PTY
        // layer also reports a TERM but some builds of claude key
        // colour fall-back off the env. Caller-supplied env wins.
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
        // Claude's TUI submits on Enter (carriage return). Newlines in
        // the user's text are passed through; we only append a CR at
        // the end so multi-line pastes don't fire prematurely.
        let mut out = Vec::with_capacity(text.len() + 1);
        out.extend_from_slice(text.as_bytes());
        out.push(b'\r');
        Bytes::from(out)
    }

    fn graceful_stop(&self) -> StopSequence {
        StopSequence(vec![
            // 1. polite in-band exit
            StopAction::SendInput(Bytes::from_static(b"/exit\r")),
            StopAction::AwaitExit(Duration::from_secs(3)),
            // 2. SIGTERM-equivalent
            StopAction::Signal(StopSignal::Terminate),
            StopAction::AwaitExit(Duration::from_secs(2)),
            // 3. hard kill
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
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut out: Vec<DiscoveredSession> = Vec::new();
        let want_root = hints.project_root.as_deref().map(canonicalize_or_keep);

        let entries = match collect_session_files(&root) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(AdapterError::Transient(format!(
                    "cannot read claude sessions dir {}: {e}",
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
            let project_hint = meta.cwd.clone().map(PathBuf::from);
            if let Some(ref want) = want_root {
                let Some(ref cwd) = meta.cwd else {
                    continue;
                };
                let got = canonicalize_or_keep(Path::new(cwd));
                if &got != want {
                    continue;
                }
            }
            let created_at = meta.timestamp.or_else(|| file_mtime_rfc3339(&file));
            out.push(DiscoveredSession {
                external_id: meta.session_id,
                project_hint,
                title_hint: None,
                external_path: Some(file.clone()),
                created_at,
            });
        }

        Ok(out)
    }
}

/// Resolve the on-disk sessions root. Honours [`SESSIONS_DIR_ENV`] first
/// (test override), then `<HOME>/.claude/projects`. Returns `None` when
/// neither is available — discover() treats that as "no sessions".
fn sessions_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(SESSIONS_DIR_ENV) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// Walk the sessions root and return paths of every `*.jsonl` session
/// rollout under it. Claude lays sessions out as
/// `<root>/<encoded-cwd>/<session-uuid>.jsonl` — we just recurse one
/// level deep, but accept anywhere in the tree to stay forward
/// compatible with shape changes.
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
                if ext.eq_ignore_ascii_case("jsonl") {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

/// First-line metadata pulled from a claude session rollout. The on-disk
/// shape varies across releases — `session_id`/`sessionId`/`id` and
/// `cwd`/`workingDir` are all observed — so the adapter accepts the
/// permissive union via aliases.
#[derive(Debug, Deserialize)]
struct SessionMetaWire {
    #[serde(alias = "session_id", alias = "sessionId", alias = "id")]
    session_id: String,
    #[serde(default, alias = "cwd", alias = "workingDir", alias = "working_dir")]
    cwd: Option<String>,
    #[serde(
        default,
        alias = "timestamp",
        alias = "created_at",
        alias = "createdAt",
        alias = "started_at",
        alias = "startedAt"
    )]
    timestamp: Option<String>,
}

/// Read the first non-empty line of a rollout file and try to extract
/// the session metadata. Returns `None` for any I/O or shape error —
/// callers skip malformed entries silently.
fn read_session_meta(path: &Path) -> Option<SessionMetaWire> {
    let bytes = std::fs::read(path).ok()?;
    for raw_line in bytes.split(|b| *b == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<SessionMetaWire>(raw_line) {
            Ok(m) => return Some(m),
            Err(_) => continue,
        }
    }
    None
}

fn canonicalize_or_keep(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Pull the first sem-ver-ish token out of `claude --version` output.
///
/// Expected shapes (observed in the wild):
///
/// - `"2.1.158 (Claude Code)\n"`
/// - `"claude 2.1.158\n"`
/// - `"Claude Code 2.1.158\n"`
///
/// We accept any whitespace-separated token of the form `N(.N)+`
/// optionally followed by `-suffix` (pre-release tags).
fn parse_version(s: &str) -> Option<String> {
    s.split_whitespace().find_map(|raw| {
        // Tolerate a leading `v` / `V` (some CLIs print `v1.2.3`).
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
        "run `claude login`",
        "run \"claude login\"",
        "auth required",
    ];
    let hay_stdout = stdout.to_lowercase();
    let hay_stderr = stderr.to_lowercase();
    needles
        .iter()
        .any(|n| hay_stdout.contains(n) || hay_stderr.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_stable() {
        let d = ClaudeAdapter::new().descriptor();
        assert_eq!(d.id, "claude");
        assert_eq!(d.default_program, "claude");
    }

    #[test]
    fn parses_real_world_version_shapes() {
        assert_eq!(
            parse_version("2.1.158 (Claude Code)\n").as_deref(),
            Some("2.1.158")
        );
        assert_eq!(
            parse_version("claude 2.1.158\n").as_deref(),
            Some("2.1.158")
        );
        assert_eq!(
            parse_version("Claude Code 2.1.158\n").as_deref(),
            Some("2.1.158")
        );
        assert_eq!(
            parse_version("v2.1.158-beta.1\n").as_deref(),
            Some("2.1.158-beta.1")
        );
        assert_eq!(parse_version("nothing here\n"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn encode_user_input_appends_cr() {
        let bytes = ClaudeAdapter::new().encode_user_input("hello");
        assert_eq!(bytes.as_ref(), b"hello\r");
    }

    #[test]
    fn encode_user_input_passes_internal_newlines() {
        let bytes = ClaudeAdapter::new().encode_user_input("line1\nline2");
        assert_eq!(bytes.as_ref(), b"line1\nline2\r");
    }

    #[test]
    fn graceful_stop_starts_with_in_band_exit() {
        let seq = ClaudeAdapter::new().graceful_stop();
        match &seq.0[0] {
            StopAction::SendInput(b) => assert!(b.windows(5).any(|w| w == b"/exit")),
            other => panic!("expected SendInput, got {other:?}"),
        }
        // last step must be a hard kill so we never hang forever
        match seq.0.last().unwrap() {
            StopAction::Signal(StopSignal::Kill) => {}
            other => panic!("expected final Kill, got {other:?}"),
        }
    }

    #[test]
    fn unauthenticated_detector() {
        assert!(looks_unauthenticated(
            "",
            "Error: not logged in. Run `claude login`."
        ));
        assert!(looks_unauthenticated("UNAUTHENTICATED\n", ""));
        assert!(!looks_unauthenticated("2.1.158 (Claude Code)", ""));
    }

    #[test]
    fn spawn_spec_default_is_interactive_no_prompt_flag() {
        let req = SpawnRequest::new("/tmp/wt");
        let spec = ClaudeAdapter::new().spawn_spec(&req).expect("spec");
        assert!(
            spec.args.is_empty(),
            "interactive spawn should not pass --print, got {:?}",
            spec.args
        );
        assert_eq!(spec.cwd, PathBuf::from("/tmp/wt"));
        assert_eq!(spec.stdin_mode, StdinMode::Pty);
    }

    #[test]
    fn spawn_spec_nullsink_with_prompt_uses_print_mode() {
        let mut req = SpawnRequest::new("/tmp/wt");
        req.stdin_mode = StdinMode::NullSink;
        req.prompt = Some("say hi".into());
        let spec = ClaudeAdapter::new().spawn_spec(&req).expect("spec");
        assert_eq!(spec.args[0], OsString::from("--print"));
        assert_eq!(spec.args[1], OsString::from("say hi"));
    }

    #[test]
    fn spawn_spec_honours_program_override() {
        let mut req = SpawnRequest::new("/tmp/wt");
        req.program_override = Some(PathBuf::from("/custom/claude"));
        let spec = ClaudeAdapter::new().spawn_spec(&req).expect("spec");
        assert_eq!(spec.program, PathBuf::from("/custom/claude"));
    }

    #[test]
    fn spawn_spec_extra_args_append_after_print() {
        let mut req = SpawnRequest::new("/tmp/wt");
        req.stdin_mode = StdinMode::NullSink;
        req.prompt = Some("q".into());
        req.extra_args = vec![OsString::from("--allowedTools"), OsString::from("Read")];
        let spec = ClaudeAdapter::new().spawn_spec(&req).expect("spec");
        let s: Vec<&OsString> = spec.args.iter().collect();
        assert_eq!(
            s,
            vec![
                &OsString::from("--print"),
                &OsString::from("q"),
                &OsString::from("--allowedTools"),
                &OsString::from("Read"),
            ]
        );
    }

    #[test]
    fn spawn_spec_sets_term_when_absent() {
        let req = SpawnRequest::new("/tmp/wt");
        let spec = ClaudeAdapter::new().spawn_spec(&req).expect("spec");
        assert!(spec.env.iter().any(|(k, _)| k == "TERM"));
    }

    #[test]
    fn spawn_spec_respects_caller_term_override() {
        let mut req = SpawnRequest::new("/tmp/wt");
        req.env = vec![(OsString::from("TERM"), OsString::from("dumb"))];
        let spec = ClaudeAdapter::new().spawn_spec(&req).expect("spec");
        let term: Vec<&OsString> = spec
            .env
            .iter()
            .filter_map(|(k, v)| if k == "TERM" { Some(v) } else { None })
            .collect();
        assert_eq!(term, vec![&OsString::from("dumb")]);
    }

    fn default_pty_hints() {
        let req = SpawnRequest::new("/tmp/wt");
        assert_eq!(req.pty, crate::PtyHints::default());
    }
    #[test]
    fn pty_hints_default() {
        default_pty_hints()
    }
}
