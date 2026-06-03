# Troubleshooting

Bugs and rough edges, sorted by where you'll hit them first. If something here doesn't match what you're seeing, please file an issue: <https://github.com/TinyWhite1997/LazyAgents/issues>.

## `la` won't start

### `daemon: missing` / `spawn-failed` in the status bar

`la` couldn't find or start the `lad` binary.

1. Check `lad` is on your `$PATH`: `which lad`. If you installed via cargo/script, it's usually `~/.cargo/bin` or `~/.local/bin`.
2. If `lad` is installed but in a non-standard location, `la` looks next to its own binary first, then `$PATH`. Symlink or move them into the same directory.
3. Run `lad doctor` to see what state dir / socket the daemon would use, and verify the runtime dir is writable.
4. Run `lad start` directly to see daemon-side startup errors instead of the auto-spawn's swallowed stderr.

### `LAZYAGENTS_NO_AUTODAEMON=1` is set somewhere

Auto-spawn is suppressed. Either unset it, or start `lad daemonize` yourself.

### Permission denied on the socket

The runtime dir is `0700` and the socket file is `0600`, owner-only by design. If you ran `lad` as a different user (e.g. with `sudo`) and now `la` runs as you, the socket isn't readable. Stop the daemon, delete `$XDG_RUNTIME_DIR/lazyagents/lad-1.sock`, and re-run.

## Daemon won't start

### Address already in use / socket file present but stale

A previous `lad` crashed without cleaning up its socket. The daemon will refuse to bind on top of a live-looking file. Confirm no daemon is running:

```sh
pgrep -f 'lad start' || rm "$XDG_RUNTIME_DIR/lazyagents/lad-1.sock"
```

### Crashes on startup without explanation

Bump the log level:

```sh
LAZYAGENTS_LOG=debug lad start 2> /tmp/lad.log
```

Logs are plain text on stderr (there is no JSON log format in v1). `LAZYAGENTS_LOG` accepts the same syntax as `RUST_LOG` — e.g. `LAZYAGENTS_LOG=la_daemon=trace,la_storage=debug`.

### SQLite migrations error

The most likely cause is a partial upgrade where the on-disk schema is newer than the binary. Confirm with:

```sh
sqlite3 "$XDG_DATA_HOME/lazyagents/lad.sqlite" 'SELECT * FROM _sqlx_migrations;'
```

If you've intentionally rolled back to an older `lad`, you need to roll back the db too (or wipe it: `rm $XDG_DATA_HOME/lazyagents/lad.sqlite*`). LazyAgents does not do automatic downgrades.

## A session won't spawn

### `unauthenticated; see <docs_url>`

Your backend CLI is not logged in. Run the CLI directly to confirm:

- `claude` → `claude login`
- `codex` → `codex login`
- `opencode` → `opencode auth login`

Then re-create the session. See [Adapters → When the adapter says "unauthenticated"](adapters.md#when-the-adapter-says-unauthenticated).

### `command not found` on spawn

The adapter expects the backend CLI to be on `$PATH`. Either install it, or pass an absolute `program_override` to `sessions.create`. A persistent per-adapter config key is on the roadmap.

### Worktree creation fails before the session starts

You'll see a `WorktreeProvision` error. Common causes:

| Error fragment | Fix |
|---|---|
| `the git binary was not found on $PATH` | Install git. |
| `'la/session-...' is already used by worktree at ...` | `git worktree prune` then `git branch -D la/session-<sid>` in the project repo. |
| `not a git repository` | Pick a project root that's actually a git repo, or run `git init`. |

The daemon enforces a 30-second timeout on every git subprocess. Repos that are slow because of network-mounted storage or a huge history can hit this; move the repo to local disk.

## Lost output / "gap" indicator

If the TUI shows `missed N bytes`, the in-memory ring buffer (2 MiB per session) overflowed before your viewer caught up. The data is **still in the persisted transcript** — only the live-replay path lost it.

For a recovery flow:

1. Detach (Esc).
2. Reattach. The TUI replays whatever's still in the ring.
3. If you need the missed bytes specifically, the transcript chunks live in `session_chunks` (≤ 8 MiB total) or `<state_dir>/sessions/<sid>.log` (spillover, JSONL with base64 payloads).

## Platform notes

### Linux

v1 is validated here. If you hit a bug, file an issue with `lad doctor` output and a `LAZYAGENTS_LOG=debug` capture.

### macOS

The release pipeline produces `x86_64-apple-darwin` and `aarch64-apple-darwin` binaries, but **macOS smoke testing is a release-blocker for GA, not Beta**. Things that *should* work: PTY spawn/read/write/signal (the `portable-pty` Unix path is shared with Linux), the worktree review, crons.

Things to watch for: code-signing prompts on first run (the binary is sigstore-attested, not Apple-notarised), and any divergence in `setsid` / process-group behaviour. File an issue with `lad doctor` output and a capture if anything misbehaves.

### Windows

The release pipeline produces `x86_64-pc-windows-msvc`. Windows ARM is intentionally out of scope for v1. Known issues from the M0 spike report:

- **EOF reporting is delayed.** Unix PTYs report EOF promptly after the child exits; Windows ConPTY can keep the reader side open beyond child exit on GitHub-hosted runners (and possibly elsewhere). This may make a session appear "still running" for a moment after the agent exited.
- **`Signal::Interrupt` may not terminate the child.** `GenerateConsoleCtrlEvent(CTRL_C_EVENT)` can succeed without the ConPTY child actually exiting. If a Ctrl-C doesn't stop the agent, follow up with `Signal::Kill` (the TUI's "force kill" path).
- **ConPTY emits extra ANSI/OSC chatter** (cursor position queries, mode-change reports). LazyAgents' VTE parser absorbs these silently, so you shouldn't see them in the transcript — but they explain why a transcript playback may look noisier than a Unix one.
- **Resize doesn't `SIGWINCH`** (there is no such signal on Windows). Console apps must poll `GetConsoleScreenBufferInfo`. Some TUI agents may not redraw cleanly on resize on Windows.

Minimum Windows version: **10 build 1809** (the first one with ConPTY).

## Config file is broken

If `config.toml` contains malformed TOML, the TUI keeps running with in-memory defaults and refuses to save UI changes (writing back would clobber whatever good content you have). Fix the TOML and re-launch. You'll see a toast on startup saying the save was refused.

## "I changed my mind, undo this archive"

There is no `sessions.unarchive` RPC in v1. The row stays in SQLite with `state='archived'` and `archived_at` set, and `sessions.list { include_archived: true }` returns it — so the data isn't lost; the TUI just doesn't yet expose a restore action.

If you need to manually un-archive while a fix lands, you can edit the row:

```sh
sqlite3 "$XDG_DATA_HOME/lazyagents/lad.sqlite" \
  "UPDATE sessions SET state='exited', archived_at=NULL WHERE id='<sid>';"
```

(Don't do this while the daemon is running on a session that's actually still archived in-memory — restart `lad` after the edit.)

## Crashes / panics

LazyAgents does not currently write crash dump files. A planned crash report feature (`crashes/<ts>.json` with the last 100 tracing events) is in the architecture doc but not in v1. For now: collect `LAZYAGENTS_LOG=trace` output and attach it to an issue.

## Cron didn't fire

Walk through it in order:

1. Is the cron **enabled**? Press Space on the row — it must be enabled, not just saved. (Editing any sensitive field auto-disables.)
2. Run `R` (dry-run) — does the next predicted fire time match your expectation?
3. Are you in the right `tz`? Crons store an IANA name; check that `Asia/Shanghai` vs `UTC` didn't catch you out.
4. Was the daemon up at the scheduled time? If not, `catchup_mode` decides: `skip` drops it, `coalesce` (default) emits one fire on wake, `replay` enqueues every missed fire.
5. Did the cron auto-pause? `consecutive_failures >= pause_on_consecutive_failures` flips it off. The TUI shows "paused after N failures".
6. Is the backend authenticated? The cron run will fail with `unauthenticated; see <docs>` if not.

## Where to look first

| Symptom | First check |
|---|---|
| `la` shows blank screen | Is your terminal in UTF-8 mode? Some emulators default to a non-UTF-8 locale. |
| Garbled colours | Try `theme = "dark"` (or `"light"`) explicitly in `[ui]` to bypass auto-detection. |
| Diff is empty even though files changed | The file might be > 5 MiB or binary — use `worktree.open_in_editor` instead. |
| Cron fired but no session | Check the `runs` table — the row carries `status`, `error_kind`, `error_detail`. |

## Get help

- Issues: <https://github.com/TinyWhite1997/LazyAgents/issues>
- When opening an issue, please include:
  - `la --version` and `lad --version`
  - `lad doctor` output
  - Your OS / terminal / Rust toolchain (if built from source)
  - A `LAZYAGENTS_LOG=debug` log capture of the failing run if possible
