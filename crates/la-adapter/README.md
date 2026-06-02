# la-adapter

Agent adapter abstraction + built-in adapters for LazyAgents.

Implements `§4.1 / ADR-002` of `report/技术架构设计.md`: a
backend-agnostic [`AgentAdapter`] trait plus concrete implementations
(currently `claude::ClaudeAdapter`). Adapters are deliberately pure —
no IPC, no SQLite, no PTY ownership — so they can be unit-tested with
`cargo test` and a stub binary (`tests/bin/mock_cli.rs`).

## Status (M0 / WEK-13)

The trait surface is complete. The Claude Code adapter implements the
M0 quartet:

| Method | M0 | Notes |
|--------|----|-------|
| `descriptor`        | ✅ | Static metadata |
| `probe`             | ✅ | Parses `claude --version`; classifies `Unauthenticated` from stderr keywords |
| `spawn_spec`        | ✅ | Returns PTY-ready `SpawnSpec`; interactive vs. `--print` decided by `StdinMode` |
| `encode_user_input` | ✅ | Appends `\r` (claude TUI submit key) |
| `graceful_stop`     | ✅ | `/exit` → SIGTERM → SIGKILL with bounded waits |
| `discover`          | trait default | M2 work |
| `parse_chunk`       | trait default = `Passthrough` | M2 work |

## Tests

```bash
cargo test -p la-adapter                       # unit + mock-cli integration
LA_RUN_CLAUDE_E2E=1 \
  cargo test -p la-adapter --test real_claude  # real claude CLI, real tokens
LA_RUN_CODEX_E2E=1 \
  cargo test -p la-adapter --test real_codex   # real codex CLI, real tokens
```

Coverage:

- `src/claude.rs` unit tests — version parsing, encode/stop semantics,
  spawn_spec branches.
- `src/codex.rs` unit tests — version parsing, encode/stop semantics,
  line-buffered JSONL `parse_chunk`.
- `tests/adapter.rs` against the in-crate `mock-cli` binary — every
  `ProbeResult` variant + `spawn_spec` overrides (claude flavor).
- `tests/codex.rs` against `mock-cli` with `MOCK_CLI_FLAVOR=codex` —
  every `ProbeResult` variant, `spawn_spec` arg ordering, `discover()`
  against a temp `CODEX_SESSIONS_DIR`.
- `tests/real_claude.rs` / `tests/real_codex.rs` — opt-in E2Es that
  drive the real CLIs through `la-pty`.

## Non-goals

- No PTY ownership — handed off to `la-pty`.
- No persistence — that's `la-storage`.
- No IPC — adapters never touch the RPC bus.
- No structured event parsing yet (`parse_chunk` is `Passthrough`); the
  trait surface is ready for M2 to specialize per backend.

## Codex (WEK-24 / M2.1)

`codex::CodexAdapter` implements the full trait surface against the
OpenAI Codex CLI (`codex` on PATH). Designed against `codex-cli 0.135.0`.

| Method | M2.1 | Notes |
|--------|------|-------|
| `descriptor`        | ✅ | id=`codex`, default_program=`codex` |
| `probe`             | ✅ | Parses `codex --version`; secondary `codex login status` probe classifies `Unauthenticated` |
| `spawn_spec`        | ✅ | `NullSink+prompt` → `exec --json --cd <cwd> <prompt>`; interactive uses TUI with optional `--cd` |
| `encode_user_input` | ✅ | Appends `\n` (codex TUI submit key) |
| `graceful_stop`     | ✅ | Signal-only (SIGTERM → wait → SIGKILL); no stable in-band exit across versions |
| `discover`          | ✅ | Walks `~/.codex/sessions/**/*.jsonl` (and legacy flat `*.json`); reads first `session_meta` line; honours `CODEX_SESSIONS_DIR` override |
| `parse_chunk`       | ✅ | Line-buffered JSONL; emits one `Passthrough` per complete line (sanity-parses each line but does not yet extract structured events) |

`parse_chunk` currently only does line-splitting plus a JSON
sanity-check — full structured event mapping (typed `AdapterEvent`
variants for `task_started`, `task_completed`, tool calls, etc.) is
deferred to a later milestone. When that lands the adapter will start
raising `AdapterError::ProtocolDrift` as the schema tightens.

### Codex CLI version compatibility

| codex CLI version | `--version` parse | `exec --json` | `login status` | sessions dir layout | adapter status |
|-------------------|-------------------|---------------|----------------|---------------------|----------------|
| 0.135.0 (current dev box) | ✅ | ✅ JSONL | ✅ classifies Unauthenticated | nested `YYYY/MM/DD/rollout-*.jsonl` | ✅ supported |
| 0.106.0 (historical, seen in archived `session_meta`) | unknown — likely same shape | unknown | unknown | same nested layout | best-effort, untested |
| < 0.100 (hypothetical older) | likely flat `~/.codex/sessions/*.json` | may lack `--json` | unknown | flat | `discover()` tolerates flat fallback; `exec` may degrade to passthrough |

`discover()` accepts both the nested 0.13x layout and the older flat
`*.json` layout in the same walk, so a workspace that has been around
through multiple codex upgrades surfaces sessions from every era.
Resume (`codex resume <id>`) is intentionally not modelled by
`spawn_spec` — the architecture doc specifies that resume always spawns
a fresh process; `discover()` exposes the external id so the daemon can
build the resume spawn elsewhere.
