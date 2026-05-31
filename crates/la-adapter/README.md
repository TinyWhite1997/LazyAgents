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
```

Coverage:

- `src/claude.rs` unit tests — version parsing, encode/stop semantics,
  spawn_spec branches.
- `tests/adapter.rs` against the in-crate `mock-cli` binary — every
  `ProbeResult` variant + `spawn_spec` overrides.
- `tests/real_claude.rs` — opt-in E2E that drives a real `claude --print`
  through `la-pty`, asserting "prompt → reply" actually round-trips
  (the WEK-13 acceptance criterion).

## Non-goals

- No PTY ownership — handed off to `la-pty`.
- No persistence — that's `la-storage`.
- No IPC — adapters never touch the RPC bus.
- No structured event parsing yet (`parse_chunk` is `Passthrough`); the
  trait surface is ready for M2 to specialize per backend.
