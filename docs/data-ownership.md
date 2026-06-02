# Data ownership: your other CLI tools (Claude / Codex / Opencode)

LazyAgents lets you see and resume sessions started directly with the
`claude`, `codex`, or `opencode` CLIs — without making copies of their
data or moving anything around on disk. This document describes exactly
what LazyAgents reads, what it writes, and what it leaves alone, so you
can decide whether the discover/import flow is right for your setup.

## Where each backend stores its sessions

LazyAgents walks the same on-disk locations the backends use natively:

| Backend  | Default discovery root           | Env override            |
| -------- | -------------------------------- | ----------------------- |
| Claude   | `~/.claude/projects/`            | `CLAUDE_SESSIONS_DIR`   |
| Codex    | `~/.codex/sessions/`             | `CODEX_SESSIONS_DIR`    |
| Opencode | `$XDG_DATA_HOME/opencode/sessions` (or `~/.local/share/opencode/sessions`) | `OPENCODE_SESSIONS_DIR` |

You can also point a single `adapters.discover` / `sessions.import`
call at a different path via the `source_path` parameter — useful for
fixtures during testing or for backends pinned to non-default storage
via `config.toml`.

## What `adapters.discover` does

`adapters.discover` walks one (or every) registered backend's
discovery root and **reads** the first JSON/JSONL record of every
session file to extract:

- the backend's own session id (`external_id`),
- the recorded working directory (`project_hint`, if any),
- a backend-provided title or first-line preview (`title_hint`),
- the recorded start time (`created_at`, falling back to the file
  mtime when the backend doesn't record one), and
- the absolute path of the file itself (`external_path`).

It does **not** modify, move, or copy any file under the discovery
root. The walk is read-only and bounded by the adapter's own file-type
filter (`*.jsonl` for claude/codex, `*.json[l]` for opencode).

The result also carries an `already_imported` flag set when the
LazyAgents database already has a `sessions` row pinned to that
`(backend, external_id)` pair — TUI renders those rows greyed-out so
they aren't offered for re-import.

## What `sessions.import` does

When you press `i` on a discovered row (or call `sessions.import`
explicitly), LazyAgents:

1. Re-runs the adapter's discover step to find the row by
   `external_id`.
2. Inserts a row in its own SQLite `sessions` table with:
   - a fresh `session_id` (UUID v7),
   - `origin = 'import'`,
   - `external_id` set to the backend's id,
   - `external_path` set to the absolute path of the backend's own
     transcript file (read-only reference, never opened for writing),
   - the recorded `created_at`, `title`, and `project_hint`.
3. Returns the new `session_id` (or the existing one when the row was
   already imported — re-importing is idempotent).

LazyAgents **never copies the transcript bytes**, and it never
modifies the backend's file. The only thing that lives in LazyAgents
storage is a pointer plus the metadata the backend already exposed.
If you delete the backend's session file from disk, the corresponding
LazyAgents row will still exist but resume will fail (the backend
itself owns the data).

## Resume semantics (M2.4+)

Resuming an imported session does **not** take over the backend's
existing PTY or process — that one is long gone. Instead the daemon
spawns a fresh process with the backend's own `--resume <external_id>`
flag (or the equivalent HTTP call for opencode), pointed at the same
`external_path`. The backend reads its data store the same way it
would from a manual `claude --resume`, and LazyAgents records the new
PTY's output into its own transcript.

This means imported sessions follow the backend's own retention rules
— if the backend rotates or prunes its session files, LazyAgents
loses the ability to resume that row at the same time the backend
does.

## Privacy / portability

- No data leaves your machine. Discovery and import are entirely
  local SQLite + filesystem reads.
- The backend's files stay where they are. Uninstalling LazyAgents
  leaves your `~/.claude/projects/`, `~/.codex/sessions/`, etc.
  untouched.
- You can re-import the same row repeatedly without creating
  duplicates: `(backend, external_id)` is enforced unique in the
  `sessions` table (migration `0003_session_external_reference.sql`).
- LazyAgents will not write into the backend's data directory. The
  only on-disk side effect of an import is the row added to
  `lad.sqlite`.

If you'd rather not have a backend's sessions surface in LazyAgents
at all, point its `*_SESSIONS_DIR` env to an empty directory or just
don't register the adapter — both paths are honoured at startup.
