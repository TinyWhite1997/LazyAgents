# Sessions

A **session** in LazyAgents is one PTY-backed run of a backend CLI (claude, codex, opencode, or a custom adapter). The daemon owns the PTY; the TUI is just a viewer. That separation is why your sessions survive terminal closes, SSH disconnects, and `la` restarts.

## Lifecycle states

Every session is in one of six states:

| State | Meaning | Glyph in TUI |
|---|---|---|
| `starting` | PTY spawned, no output yet. Promoted to `running` automatically after 250 ms of silence or the first output byte. | `●` |
| `running` | Receiving or sending output. | `●` |
| `waiting` | More than 2 seconds since the last PTY byte; the backend is idle, waiting for you. Only sessions whose stdin is the PTY (interactive) can enter this state. | `⏸` |
| `exited` | Child process is gone. The transcript is preserved. | `·` |
| `errored` | Reserved for adapter-level failures. Not currently emitted. | `✗` |
| `archived` | Soft-deleted. Hidden from `sessions.list` unless you ask for it. | (in the Archived bucket) |

Transitions you'll see day-to-day:

- `starting → running`: the agent starts producing output.
- `running ↔ waiting`: the agent went idle (and came back when you typed).
- `running → exited`: the agent ran `exit`, you killed it, or it crashed.
- `exited → archived`: you pressed `a` to clear it out of the active list.

## Create a session

### Register a project first (Shift+N)

The Sessions sidebar groups runs by their on-disk **project directory**. To seed a brand-new project (so a subsequent `n` has something to attach a session to), press **`Shift+N`** anywhere on the Sessions sidebar — including an empty workspace, which is the only way to bootstrap the very first project.

The New-project modal takes one field: a directory path.

- Default starting path is your home directory; edit it from there.
- `~` at the start expands to `$HOME` (`%USERPROFILE%` on Windows).
- Hidden directories (`.git`, `.cache`, …) are filtered from completions unless you explicitly start the prefix with `.`.
- Only **directories** are listed — never regular files.
- The modal does **not** create directories. If the path does not exist, the modal stays open with a clear error so you can fix it.

Key map inside the modal:

| Key | Action |
|---|---|
| any printable char | Append to the path buffer. `q`/`?`/digits/letters are all literal here. |
| `Backspace` | Pop the last char from the buffer. |
| `Tab` / `↓` | Highlight the next completion candidate. |
| `Shift+Tab` / `↑` | Highlight the previous candidate. |
| `→` | Apply the highlighted candidate to the path and stay open (descend). |
| `Enter` | Two-stage: when a candidate is highlighted, descend into it; otherwise create the project. |
| `Esc` | Cancel — no project is created. |

On success the new (empty) project group lands at the top of the sidebar, the cursor is pre-positioned on its header, and the toast shows `project added: <path>`. The project is persisted immediately: the TUI calls the daemon's `projects.create` RPC, which inserts the `projects` row up front, so the empty project survives a daemon or `la` restart even before you spawn a session under it.

### From the TUI (v1 status)

The Sessions tab in v1 ships the live navigation, sidebar, and modals — and the **New-session form is wired end-to-end**: pressing **`n`** on a project opens a modal that lets you pick a backend and toggle the worktree flag, then **`Enter`** calls `sessions.create` on the daemon. The session is created with no initial prompt — you type your first instruction into the live agent after attaching. The freshly minted session appears on the sidebar within the next ~2 s refresh tick.

**Live attach is wired:** highlighting a session row and pressing **`Enter`** opens a live PTY pane backed by `sessions.attach { acquire_input: true }`. The daemon streams `session.output` chunks into a VT100 grid emulator that renders the agent's full-screen TUI faithfully, and every keystroke you type goes back through `sessions.write`. The pane reports its size to the daemon with `sessions.resize` so the agent reflows to your window.

The New-session modal field map:

| Field | Required | Notes |
|---|---|---|
| Project | yes | Captured from the sidebar selection — pick a project row before pressing `n`. |
| Backend | yes | One of `App::backends` reported as `Available` by the daemon health probe. `←`/`→` cycle the choice. |
| Worktree | no (default off) | `Space` toggles. If on, `git worktree add -b la/session-<short-sid> <base>` runs before the spawn. |
| Args | reserved | Plumbed through the trait for forward-compat; not exposed in the modal yet. |

Key map inside the modal:

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Cycle focus across Backend → Worktree. |
| `←` / `→` | Move the backend cursor (Backend field). |
| `Space` | Toggle the worktree flag (Worktree field). |
| `Enter` | Create the session via `sessions.create`. |
| `Esc` | Cancel — closes the modal, draft discarded. |

A validation slip (no available backend) keeps the modal open and stamps the error inline so you can fix it without retyping. A daemon-side refusal closes the modal and surfaces the reason via the status bar toast.

### Programmatically (JSON-RPC over the daemon socket)

```json
{"jsonrpc":"2.0","id":1,"method":"sessions.create","params":{
  "project_dir": "/home/alice/code/myapp",
  "backend":     "claude",
  "worktree":    true,
  "prompt":      "Add a README about the build system."
}}
```

Response includes the `session_id` (UUID v7), the resolved `cwd` (which is the worktree path if `worktree: true`), and the initial PTY size (`32 × 120`). The session state on return is always `starting`.

## Attach, detach, list

| TUI key | Effect |
|---|---|
| `j` / `k` / arrow keys | Move the cursor in the session list. |
| `Enter` | Attach to the highlighted session (live PTY pane opens; daemon owns input). |
| `Ctrl+\` | Detach and return to the sidebar (the session keeps running on the daemon). |
| Any other key | Forwarded verbatim to the daemon as PTY input — including arrows, PgUp/PgDn, Home/End, function keys, and Ctrl chords. The session pane is a full terminal emulator, so the agent process owns the pane and its own scrolling. |
| `q` (in the sidebar) | Quit `la`. Sessions and the daemon stay alive. |

The attach pane runs a real VT100 grid emulator: full-screen agent TUIs
(Claude Code, Codex, OpenCode) render exactly as they would in a native
terminal — cursor addressing, clear-screen, alternate screen, and colors
are all honored. The pane is sized to your window and the daemon's PTY is
resized to match (via `sessions.resize`) on attach and whenever you resize
the terminal. Because every key except `Ctrl+\` is forwarded verbatim, a
literal `Ctrl+\` (SIGQUIT) cannot be sent into the agent from the pane.

**Detach vs quit:** `Ctrl+\` releases your viewer; the daemon eagerly drops your `acquire_input` ownership via `sessions.detach`. Quitting `la` does the same plus shuts down the TUI process. Neither stops the session.

**Reattach:** when you re-open `la`, the daemon replays everything in its in-memory ring buffer (2 MiB per session) on attach so you catch up to "now". Output beyond that is in the persisted transcript (see below) but isn't streamed back automatically.

## Where the output goes

LazyAgents records every PTY chunk — your input, the agent's output, even adapter events — into SQLite. The schema makes the transcript queryable and lets you replay any contiguous range.

- For the first **8 MiB** of a session, chunks live in the `session_chunks` table inside `lad.sqlite`.
- After that, the daemon **spills to a file** at `<state_dir>/sessions/<session_id>.log` — newline-delimited JSON, one record per chunk, base64-encoded payload. Spill files are uncompressed (cron run archives are compressed; transcripts are not).
- The daemon also keeps a per-session **2 MiB ring buffer** in memory for fast replay on reattach.

Spill files are not safe to edit by hand. If you want a "save my transcript" feature, use `sessions.replay` from the RPC.

## Replay

Two ways to ask for output the daemon already has:

1. **Reattach replay (the common case).** `sessions.attach` with `resume_from_seq: <last_seq>` asks the daemon to replay only chunks newer than the one you last saw, then keep streaming live. The TUI does this for you on every reconnect.
2. **Explicit replay.** `sessions.replay` with `{ session_id, from_seq, max_bytes? }` queues a range of past output as `session.output` notifications. Useful for tools that want to fetch a historical slice without resetting the live cursor.

If the ring buffer evicted some bytes before your viewer caught up, the daemon emits a `session.gap` notification with the dropped seq range and byte count. The TUI surfaces this as a "missed N bytes" indicator — the data still exists in the transcript, but the ring can't replay it.

## Resize

Sessions ship at `32 rows × 120 cols`. When you resize your terminal, the TUI re-pins its view and (in a future release) calls `sessions.resize` to push the new size into the PTY. The PTY layer fully supports this on Unix (`TIOCSWINSZ` + `SIGWINCH`) and Windows (`ResizePseudoConsole`); only the daemon-side RPC dispatcher is still being wired through, so child apps that depend on `SIGWINCH` will not redraw mid-session in v1. Restart the session if the geometry needs to change.

## Archive vs delete

Both refuse to touch a session that's still in the active registry — you must stop it first (`sessions.signal` with `TERM` or `KILL`, or just type `exit` to the agent).

| | Archive (TUI `a`) | Delete |
|---|---|---|
| Row removed from SQLite | no | yes (cascades to transcript chunks) |
| Transcript chunks removed | no | yes |
| `.log` spill file removed | no | no (orphaned on disk) |
| Worktree directory removed | yes (best-effort) | no |
| Worktree branch removed | only if it has no commits beyond base | no |
| Reversible | row remains; restoration is roadmap | no |

**Default to archive.** Delete is for transcripts you actively want gone. There is no GC on orphaned spill files in v1 — if you `sessions.delete` a session that spilled, you should clean up `<state_dir>/sessions/<sid>.log` by hand.

## Discover and import existing sessions

LazyAgents can surface sessions you started directly with `claude`, `codex`, or `opencode` — without copying anything. The discovery walk is read-only.

The daemon side is fully wired (`adapters.discover` + `sessions.import`), and the TUI emits an `ImportDiscovered` action on the `i` key — the live import overlay is the same UI work as the New-session form. Until then, drive the import over JSON-RPC.

After import, the session shows up alongside native LazyAgents sessions, and "resuming" it (planned for the same release) will spawn a fresh backend process with the right resume flag, pointed at the original transcript file (which LazyAgents never modifies).

Discovery roots (and how to override them):

| Backend | Default path | Env override |
|---|---|---|
| Claude | `~/.claude/projects/` | `CLAUDE_SESSIONS_DIR` |
| Codex | `~/.codex/sessions/` | `CODEX_SESSIONS_DIR` |
| OpenCode | `$XDG_DATA_HOME/opencode/sessions` | `OPENCODE_SESSIONS_DIR` |

Already-imported rows are flagged so the TUI greys them out. Re-importing is idempotent. Full data-ownership rules are in [`docs/data-ownership.md`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/data-ownership.md).

## Hooks

LazyAgents looks for an executable at `<project_root>/.lazyagents/hooks/post-create.sh` and runs it after a successful **worktree-backed** session spawn (i.e. when `sessions.create` was called with `worktree: true`). The hook gets 60 seconds; failure is advisory and does not abort the session. Use it for per-project setup like seeding env files or warming caches.

Sessions created without a worktree skip the hook — there's no per-session directory to operate on.

## UI preferences (theme, compact, key hints)

The TUI's `[ui]` section lives in `$XDG_CONFIG_HOME/lazyagents/config.toml`:

```toml
[ui]
theme = "auto"        # any built-in or custom theme id (see below)
key_hints = "rich"    # rich | compact | hidden
compact = false
```

You can edit the file by hand, or use the in-TUI keys:

| Key | Effect |
|---|---|
| `T` | Open the theme picker (live preview) |
| `H` | Cycle key hints: rich → compact → hidden |
| `C` | Toggle compact layout |

### Themes

Press `T` to open the theme picker. Use `↑`/`↓` (or `k`/`j`) to move the
highlight — the whole UI previews the theme live — then `⏎` to apply and
persist, or `Esc` to revert to the theme you started with.

Built-in theme ids:

- `auto` — defers the background to your terminal (only accents are themed)
- `dark`, `light`
- `catppuccin-latte`, `catppuccin-frappe`, `catppuccin-macchiato`, `catppuccin-mocha`
- `gruvbox-dark`, `gruvbox-light`
- `nord`, `dracula`, `tokyo-night`
- `solarized-dark`, `solarized-light`

Every named theme paints its own background canvas; `auto` leaves the
canvas to your terminal.

### Custom themes

Define your own palettes with `[[ui.custom_theme]]` blocks — they appear
in the picker alongside the built-ins. A custom theme whose `id` matches a
built-in overrides it. Colours are `#rrggbb` hex; `label` defaults to `id`
and `on_accent` (text on a coloured chip) defaults to `bg`:

```toml
[[ui.custom_theme]]
id = "my-theme"
label = "My Theme"
bg = "#1e1e2e"
fg = "#cdd6f4"
muted = "#a6adc8"
primary = "#89b4fa"
ok = "#a6e3a1"
warn = "#f9e2af"
error = "#f38ba8"
on_accent = "#1e1e2e"
```

A custom theme with a missing or malformed colour is skipped — the TUI
never refuses to start over a config typo.

Changes write through to `config.toml` immediately via an atomic rename. Any other sections (`[daemon]`, `[scheduler]`, `[adapters.*]`) you've added by hand are preserved verbatim — as are your `[[ui.custom_theme]]` blocks, which the picker never edits.

## Backup

```sh
lad backup --output ./lad-snapshot.sqlite
```

Uses SQLite's Online Backup API, so it's safe while the daemon is running. The snapshot is a single file — no WAL or SHM sidecars. Copy it offsite to back up every session row, transcript chunk, cron, and run. (Spill files at `<state_dir>/sessions/*.log` and worktree directories are not in the snapshot; back them up alongside if you need them.)

## RPC reference

For tools and scripts, the daemon speaks JSON-RPC 2.0 over a length-prefixed UDS / named-pipe. Method names you'll care about for sessions:

- `sessions.list`, `sessions.create`, `sessions.attach`, `sessions.detach`
- `sessions.write`, `sessions.resize`, `sessions.signal`
- `sessions.archive`, `sessions.delete`
- `sessions.import`, `sessions.replay`
- `projects.list`, `projects.create` — enumerate projects (including empty ones) and register a directory as a project without spawning a session
- `adapters.discover`
- `events.subscribe` with topics: `session.output`, `session.state`, `session.gap`

JSON Schemas for every params/result are checked into [`docs/schema/`](https://github.com/TinyWhite1997/LazyAgents/tree/main/docs/schema) and verified against the wire types in CI on every PR.
