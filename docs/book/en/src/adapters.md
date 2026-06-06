# Adapters

An **adapter** is a thin Rust shim that teaches LazyAgents how to talk to one backend CLI. It does five things:

1. **Describe** the backend (`id`, default executable name).
2. **Probe** the executable (is it installed? is it logged in?).
3. **Build a spawn spec** (which argv and stdin mode to use, depending on whether you're starting interactive or with a one-shot prompt).
4. **Encode user input** (which byte the backend treats as "submit" — `\r` for claude, `\n` for codex and opencode).
5. **Discover existing sessions** on disk so they can be imported.

Adapters are pure code: they don't own PTYs, don't write to SQLite, and don't touch IPC. That makes them easy to unit-test against a fake CLI binary, and easy to add new ones.

## Adapters shipped in v1

| Adapter id | Wraps | Default executable |
|---|---|---|
| `claude` | Anthropic Claude Code | `claude` |
| `codex` | OpenAI Codex CLI | `codex` |
| `opencode` | sst.dev OpenCode | `opencode` |

The default executable is looked up on `$PATH`. v1 has no wire-level or `config.toml` knob to point an adapter at a non-default binary — the adapter's `SpawnRequest::program_override` field exists in the Rust API (and is exercised by the test-only `--test-shell-adapter` flag on a debug build of `lad`), but `sessions.create`'s wire schema does not yet plumb it through, and the daemon does not read an `adapters.*.command` config. If you need to redirect to a beta build or wrapper script today, symlink it onto `$PATH` ahead of the real binary. Persistent per-adapter config is a follow-up.

## Authentication: LazyAgents never logs you in

This is intentional. LazyAgents does not store backend credentials, and it does not implement login flows. Authentication is owned by the backend CLIs — `claude login`, `codex login`, `opencode auth login`. Whatever session the CLI maintains in its own config (usually `~/.claude/`, `~/.codex/`, or `~/.config/opencode/`) is what LazyAgents inherits when it spawns the CLI.

This means: if your `claude` works in a terminal, it works in LazyAgents. If `claude --version` says you're not logged in, LazyAgents will say the same.

## When the adapter says "unauthenticated"

Every adapter runs a two-phase probe on startup and on demand:

1. Run `<executable> --version`. Scan stdout/stderr for auth needles ("not logged in", "please log in", "unauthenticated", "no credentials", etc.).
2. If needed, run a secondary probe: `codex login status` or `opencode auth list`.

If a needle matches, the probe returns `Unauthenticated { docs_url }`. The TUI surfaces this with a help link to the relevant docs site. The `AdapterError::Unauthenticated` variant formats as `unauthenticated; see <docs_url>`.

| Adapter | docs_url | Fix |
|---|---|---|
| `claude` | <https://docs.claude.com/en/docs/claude-code> | `claude login` |
| `codex` | <https://developers.openai.com/codex/cli> | `codex login` |
| `opencode` | <https://opencode.ai/docs/> | `opencode auth login` |

After you re-authenticate in your terminal, the daemon's next probe will see it. There's no LazyAgents-side cache to invalidate.

The probe is **conservative**: only an explicit "no credentials" / "not logged in" keyword classifies as unauthenticated. A non-zero exit code without a keyword stays `Available`, because some CLIs return non-zero for unrelated reasons (network blip, container issues) and we don't want to falsely tell you to log in again.

## Session discovery (`adapters.discover`)

This is the read-only walk that surfaces sessions you started outside LazyAgents.

**Params:**

```json
{
  "backend":      "claude" | "codex" | "opencode" | null,   // null = walk all
  "source_path":  null,    // override the adapter's default sessions root (testing/fixtures)
  "project_root": null     // filter to sessions whose cwd matches
}
```

**Result entries:**

```json
{
  "backend":          "claude",
  "external_id":      "<the backend's own session id>",
  "external_path":    "/home/alice/.claude/projects/.../<uuid>.jsonl",
  "project_hint":     "/home/alice/code/myapp",
  "title_hint":       null,                  // opencode populates; claude/codex don't
  "created_at":       "2026-05-30T12:34:56Z",
  "already_imported": true                   // daemon-side: row already exists for (backend, external_id)
}
```

Per-adapter discovery roots:

| Adapter | Default root | Env override | File globs |
|---|---|---|---|
| `claude` | `~/.claude/projects/` | `CLAUDE_SESSIONS_DIR` | `*.jsonl` |
| `codex` | `~/.codex/sessions/` | `CODEX_SESSIONS_DIR` | `*.jsonl`, `*.json` (legacy flat layout supported) |
| `opencode` | `$XDG_DATA_HOME/opencode/sessions` | `OPENCODE_SESSIONS_DIR` | `*.json`, `*.jsonl` |

The walk is bounded to the file-type globs above; it does not chase symlinks outside the root, and it does not mutate or copy anything. See [`docs/data-ownership.md`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/data-ownership.md) for the full data-ownership rules.

## Importing a discovered session

> **v1 status.** Daemon-side `adapters.discover` and `sessions.import` are wired end-to-end (the `(backend, external_id)` uniqueness constraint is enforced in migration `0003_session_external_reference.sql`). The **TUI import overlay is not yet wired** — pressing `i` today only flips a flag on the TUI's mock `SessionSource`; it does not yet call `sessions.import`. To actually import in v1, drive the RPC pair over the IPC socket.

### v1 path: JSON-RPC

```json
{"jsonrpc":"2.0","id":1,"method":"adapters.discover","params":{"backend":"claude"}}
{"jsonrpc":"2.0","id":2,"method":"sessions.import","params":{
  "backend":      "claude",
  "external_ids": ["<external_id-from-discover>"]
}}
```

`external_ids` is a list — pass one or many. Omitting it imports every session the adapter currently discovers; unknown ids are silently dropped so a stale snapshot never wedges the call.

The import handler creates a fresh LazyAgents row with `origin = "import"`:

- a fresh `session_id` (UUID v7),
- `external_id` set to the backend's id,
- `external_path` pointing at the original transcript file (LazyAgents only ever reads from this path),
- `created_at`, `title`, `project_hint` from the backend's first-line metadata.

Re-importing the same `(backend, external_id)` is a no-op — uniqueness is enforced in the schema (migration `0003_session_external_reference.sql`).

The TUI `i` keystroke will round-trip to the same RPC pair once the import overlay lands; until then it manipulates local mock state only.

## Resuming an imported session

Resume is on the roadmap and is **not yet wired** in v1. When it lands, it will not take over the backend's old PTY (that one is long gone) — instead the daemon will spawn a fresh process with the backend's own resume flag, pointed at the `external_path` you discovered earlier. The backend reads its own transcript file the same way it would from a manual `claude --resume`, and LazyAgents records the new PTY's bytes alongside.

The planned resume invocations:

| Adapter | Resume invocation (planned) |
|---|---|
| `claude` | `claude --resume <external_id>` |
| `codex` | `codex resume <external_id>` |
| `opencode` | `opencode run --session <external_id>` / `--continue` |

Imported sessions will follow the backend's own retention rules — if the backend prunes or rotates the file, resume stops working for that row at the same moment.

## Per-adapter notes

### `claude` (Anthropic Claude Code)

- Non-interactive flag: `--print`.
- TUI submit byte: `\r`.
- Graceful stop: in-band `/exit\r` → SIGTERM → SIGKILL with bounded waits.
- Discovery layout: `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`. First-line JSON record carries `session_id` / `sessionId` / `id`, `cwd` / `workingDir`, `timestamp` / `created_at`.
- `title_hint` not exposed by claude.

### `codex` (OpenAI Codex CLI)

- Designed against `codex-cli 0.135.0`.
- Non-interactive flag: `exec --json --cd <cwd> <prompt>`.
- TUI submit byte: `\n`.
- Graceful stop: signal-only (SIGTERM → wait → SIGKILL). Codex doesn't have a stable in-band exit command across versions.
- Discovery layout: nested `YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` (current) or flat `*.json` (legacy). The discover walk tolerates both.
- First-line JSON records have a `kind: "session_meta"` wrapper around the payload.
- `title_hint` not exposed by codex.

### `opencode` (sst.dev OpenCode)

- Designed against `opencode 1.2.15`.
- Non-interactive flag: `run --format json [--dir <cwd>] <prompt>`.
- TUI submit byte: `\n`.
- Graceful stop: signal-only.
- Discovery layout: flat `$XDG_DATA_HOME/opencode/sessions/*.json` (and `.jsonl`). Tolerates both bare-payload and nested-envelope (`meta` / `session` / `payload` keys) shapes.
- `title_hint` is populated from `meta.title` — opencode is the only adapter that exposes a session title.

## Adapter discovery from the IPC

For tools and scripts:

```json
{"jsonrpc":"2.0","id":1,"method":"adapters.discover","params":{"backend":"claude"}}
```

JSON Schemas:
- params: [`docs/schema/adapters__discover.params.schema.json`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/schema/adapters__discover.params.schema.json)
- result: [`docs/schema/adapters__discover.result.schema.json`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/schema/adapters__discover.result.schema.json)

A schema check in CI fails the build if either drifts from the wire types — what you see in the schema is what the daemon will accept.
