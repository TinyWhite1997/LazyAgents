# FAQ

## General

### What is LazyAgents *not*?

- Not a backend itself. It drives `claude`, `codex`, or `opencode` — you still need one of them installed and logged in.
- Not a network service. No TCP listener, no telemetry, no upload. Everything stays on your machine.
- Not a replacement for tmux. Tmux is general-purpose; LazyAgents is specifically for coding agents and gives you sessions + crons + worktrees as first-class concepts.
- Not a multi-user service. Sockets are `0600` owner-only, and the daemon enforces `SO_PEERCRED` on accept. One user, one daemon.

### Is there a hosted version?

No. LazyAgents is local-only by design.

### Is it stable?

It's v1, validated on Linux. Treat macOS and Windows as Beta-quality until GA. See [Install → v1 platform scope](install.md) and [Troubleshooting → Platform notes](troubleshooting.md#platform-notes).

## Installation & updates

### Why no Scoop bucket?

cargo-dist 0.32 (what we use to build releases) ships native installer support for `shell`, `powershell`, `npm`, `homebrew`, and `msi` — there's no first-party Scoop generator. Shipping a Scoop manifest cleanly needs either a custom publish job or a third-party tool, and that's wider than v1 scope. Use the PowerShell installer on Windows. Scoop support is tracked as a follow-up.

### Why no Homebrew tap installs work yet?

The Homebrew formula is generated on every release, but the `TinyWhite1997/homebrew-tap` repository hasn't been published. Until that flips on, use `install.sh`.

### Will it auto-update?

No. `la --check-update` queries GitHub Releases and tells you when there's a newer build — re-run the installer to upgrade. Auto-update via cargo-dist is explicitly disabled (`install-updater = false`).

### I run a self-hosted fork / my network can't reach `api.github.com`

Set `LAZYAGENTS_UPDATE_MANIFEST_URL` to point at your own mirror's GitHub-Releases-compatible endpoint, and `la --check-update` will query that instead. The endpoint just needs to return JSON with `tag_name`, `html_url`, and `prerelease`. See [`docs/install.md#forks--air-gapped-networks-lazyagents_update_manifest_url`](../../../install.md#forks--air-gapped-networks-lazyagents_update_manifest_url) for the full example.

### How do I verify the binary I downloaded?

```sh
gh attestation verify ./<artifact> --repo TinyWhite1997/LazyAgents
```

Releases are signed via sigstore-backed GitHub Artifact Attestations. There is no separate cosign signature to verify — the attestation *is* the sigstore record.

## Sessions

### Do my sessions survive a reboot?

The daemon does not auto-respawn on reboot (no systemd unit is installed). After a reboot, run `la` and it'll auto-spawn `lad`, which re-seeds from SQLite. Active sessions whose PTY children were killed by the reboot get marked `exited`; their transcripts are preserved.

If you want the daemon to come back on its own, write your own user systemd unit calling `lad start` — this is on the roadmap but not in v1.

### What happens to a session when I close the terminal?

Nothing. The PTY child is owned by `lad`, not your terminal. Quitting `la` (or your terminal dying) only detaches the viewer.

### Can I attach to a session from a different machine?

No. v1 is local-only. The daemon binds a Unix socket / Named Pipe with `SO_PEERCRED` UID checking; there is no network endpoint.

### Can two `la` instances watch the same session at once?

Yes. Multiple subscribers to the same session each get a fan-out of `session.output` notifications from the daemon's bus. Useful when you want a "watch" pane in one terminal and an attach in another.

### Where exactly is my data?

| Thing | Path (Linux defaults) |
|---|---|
| SQLite db | `~/.local/share/lazyagents/lad.sqlite` |
| Spilled transcripts | `~/.local/share/lazyagents/sessions/<sid>.log` |
| Worktrees | `~/.local/share/lazyagents/worktrees/<project>/<sid>/` |
| Cron run archives | `~/.local/share/lazyagents/runs/archive/<yyyymm>.jsonl.zst` |
| UI config | `~/.config/lazyagents/config.toml` |
| Socket | `/run/user/<uid>/lazyagents/lad-1.sock` |

Override `LAZYAGENTS_DATA_DIR`, `LAZYAGENTS_CONFIG_HOME`, `LAZYAGENTS_RUNTIME_DIR` to move them.

### Can I import sessions I already started with `claude`/`codex`/`opencode`?

Yes — read-only. The discover walk surfaces them; press `i` in the import overlay to import. LazyAgents stores only a pointer (`external_path`), never a copy. See [Sessions → Discover & import](sessions.md#discover-and-import-existing-sessions) and [`docs/data-ownership.md`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/data-ownership.md).

### Why can't I `sessions.delete` a running session?

By design — `delete` is a hard CASCADE in SQLite. The dispatcher refuses if the session is still in the active registry (state `starting`, `running`, or `waiting`). Stop it first (`sessions.signal` with `TERM` or `KILL`, or have the agent `exit`), then delete.

### Will the transcript file in `sessions/` be cleaned up when I delete a session?

No, not in v1. `sessions.delete` removes the row and CASCADEs the `session_chunks` rows, but the spilled `.log` file is orphaned on disk. Clean those up by hand if you care about disk usage.

## Crons

### How is "cron" different from a bash + cron-tab setup?

- LazyAgents understands IANA timezones with DST take-first semantics (see [Crons → Timezone and DST](crons.md#timezone-and-dst)).
- Each fire spawns a real LazyAgents session — you get the same transcript, attach, archive, and worktree integration as for any session.
- Runs are quota-gated (max concurrent, max per day, daily cost budget) and auto-paused after consecutive failures.
- Catch-up policies handle "the daemon was asleep" without flooding on wake.

If you don't need any of that, classic `cron` + a shell script is simpler.

### What protects a freshly-saved cron from going live by accident?

In v1, **the only protection is that you have to call `crons.set_enabled` explicitly**. `crons.upsert` for a brand-new id lands it disabled, but updating an *already-enabled* cron — even when you change backend, schedule, prompt, args, or budget — keeps it enabled and the next scheduled fire will use the new values. The daemon's `cron_security` module (`crates/la-daemon/src/cron_security.rs`) defines a `SENSITIVE_CRON_FIELDS` allowlist and a confirmation-token + summary helper (5-minute TTL, single-use, 64 KiB `MAX_PROMPT_BYTES` prompt cap) that *will* auto-disable + token-gate enabling, but those helpers are not yet plumbed into the dispatcher in v1. If you're scripting risky edits, call `set_enabled { enabled: false }` first.

### Can I put credentials in a cron prompt?

Don't. Prompts are stored as plaintext in SQLite (not encrypted, not treated as a secret). LazyAgents specifically does not use `SecretString` for them because the daemon needs to diff and persist prompt text.

### Why is my cron-launched session "non-interactive"?

Cron-triggered sessions are non-interactive by default — input ownership requires you to explicitly attach. This is a security choice (`docs/security.md`): an unattended fire that opens a writable stdin is a way for prompt-injection in the agent's output to start affecting later sessions.

## Worktrees

### Does LazyAgents speak git natively or shell out?

Shells out to the `git` binary. No libgit2. Requires git ≥ 2.20 for predictable English error messages (the classifier uses pattern matching).

### Why does `worktree.discard` need `confirmed: true`?

Fail-closed default. Older clients that don't know about the field get a `WORKTREE_DISCARD_UNCONFIRMED` error instead of silently throwing away your work.

### Can I `--amend` from the TUI?

Not in v1. `worktree.commit` only supports `message` and `allow_empty`. Amending an agent's in-progress branch is a footgun.

### What happens to my worktree when I archive the session?

The worktree directory is removed. The branch stays if it has commits beyond `base_branch` so you don't lose work. A background sweep deletes both after the retention TTL (7 days default).

`sessions.delete` does **not** touch the worktree — by design, since "delete" cascades in SQLite and we don't want it to silently destroy un-merged commits.

## Adapters

### How do I add a new adapter?

The `AgentAdapter` trait is in `crates/la-adapter/src/lib.rs`. A new adapter needs: `descriptor`, `probe`, `spawn_spec`, `encode_user_input`, `graceful_stop`, optionally `discover` and `parse_chunk`. Look at `crates/la-adapter/src/{claude,codex,opencode}.rs` for working examples. No IPC or storage knowledge is needed — adapters are pure.

### Does LazyAgents handle login for me?

No, and it never will. Authentication is owned by the backend CLI. If your `claude` works in a terminal, it works in LazyAgents.

### Can I rate-limit per backend?

Crons have per-cron and global quotas (max concurrent, max-per-day, daily USD budget). There is no per-backend / per-API global limiter in v1 — file an issue if you need one.

## Privacy & security

### Does LazyAgents talk to the internet?

Only on explicit user action:
- `la --check-update` hits GitHub Releases.
- The release installer downloads from GitHub.
- Your backend CLI (claude / codex / opencode) talks to its own provider — that's outside LazyAgents.

No telemetry, no usage reporting.

### Can a different user on my machine read my sessions?

No, with caveats:
- The runtime dir is `0700`, the socket is `0600`, and the daemon checks `SO_PEERCRED.uid == geteuid()` on accept. A different OS user cannot connect.
- SQLite file permissions follow your umask — `0600` is typical. Anyone with read access to `~/.local/share/lazyagents/lad.sqlite` can read your transcripts. Don't share your home directory.
- Worktree directories follow standard umask. Same rule.

### What about root?

Root can read anything. LazyAgents doesn't try to defend against that.

### Are prompts in cron definitions encrypted?

No. They're stored as plaintext in SQLite. Treat them like you'd treat the comments in your shell scripts: don't put credentials in them.

## Observability

### Is there structured logging?

Yes. `lad` writes newline-delimited JSON tracing events to stderr. Control verbosity with `LAZYAGENTS_LOG` or `lad --log-level` (same filter syntax as `RUST_LOG`).

### Is there a Prometheus endpoint?

Yes. `lad metrics` scrapes the active daemon over a local Unix-domain socket and prints Prometheus text for RPC, session, cron, PTY spawn, and storage-write metrics.

### Where are health events?

The daemon broadcasts a `daemon.health` notification topic; subscribe via `events.subscribe`. It carries periodic probe results for each registered adapter so a TUI can show "claude OK, codex unauthenticated" in real time.

## When something goes wrong

### Where do I file bugs?

<https://github.com/TinyWhite1997/LazyAgents/issues> — please include `lad doctor` output, `la --version` / `lad --version`, and a `LAZYAGENTS_LOG=debug` capture if you can.
