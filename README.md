# LazyAgents

> Keep your coding agents alive across reboots, restarts, and SSH disconnects — without nailing your laptop to a tmux session.

LazyAgents is a long-running local **daemon** (`lad`) and a fast **TUI** (`la`) that babysits unattended coding agents — [Claude Code](https://docs.claude.com/en/docs/claude-code), [OpenAI Codex](https://developers.openai.com/codex/cli), [sst.dev OpenCode](https://opencode.ai/docs/) — for you. Sessions survive your terminal closing, cron jobs run them while you sleep, and a built-in per-session git worktree lets you stage and commit what the agent produced.

---

## Install in 5 minutes

Pick one — these are the cargo-dist generated installers; both verify the artifact attestation against sigstore before dropping `la` and `lad` onto your `$PATH`.

### Linux / macOS

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/TinyWhite1997/LazyAgents/releases/latest/download/lazyagents-installer.sh | sh
```

### Windows (PowerShell)

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/TinyWhite1997/LazyAgents/releases/latest/download/lazyagents-installer.ps1 | iex"
```

### From source (any platform)

```sh
git clone https://github.com/TinyWhite1997/LazyAgents.git
cd LazyAgents
cargo install --path crates/la-daemon --locked
cargo install --path crates/la-tui    --locked
```

Then launch the TUI — you should land on the **Crons** tab, and the status bar at the bottom should read `daemon @ <socket-path>`:

```sh
la
```

The Crons tab is the production reference UI for v1; on a fresh install with no crons yet it renders like this:

```text
┌─ LazyAgents ──────────────────────────────────────────────────────────────┐
│  Projects   Sessions   [ Crons ]   Worktree                               │
├───────────────────────────────────────────────────────────────────────────┤
│  ENABLED  NAME              SCHEDULE            TZ              NEXT FIRE │
│  ─────────────────────────────────────────────────────────────────────────│
│           (no crons yet — press `n` to create one)                        │
│                                                                           │
│                                                                           │
├───────────────────────────────────────────────────────────────────────────┤
│  n new   e edit   ␣ enable/disable   d delete   r run-now   ? help        │
│  daemon @ /run/user/1000/lazyagents/lad-1.sock                            │
└───────────────────────────────────────────────────────────────────────────┘
```

A full-fidelity screenshot of the same tab once the New-session form lands is tracked as a v1 GA polish item.

Full install path, including service install (`systemd` / `launchd` / Windows Scheduled Task) and the macOS configuration fallback chain, is in [`docs/install.md`](docs/install.md). Already on LazyAgents and bumping to a new release? See [`docs/upgrade.md`](docs/upgrade.md).

> **Fork / offline / internal mirror?** `la --check-update` reads the manifest URL from the `LAZYAGENTS_UPDATE_MANIFEST_URL` env var, falling back to GitHub Releases. Point it at your own GitHub-compatible Releases endpoint (response must carry `tag_name` / `html_url` / `prerelease`) when the default host is unreachable. See [`docs/install.md` → Update / uninstall](docs/install.md#update--uninstall).

---

## Why LazyAgents

**You run coding agents long enough that babysitting a single terminal becomes a chore.** Claude Code, Codex, and OpenCode are great in the foreground, but the moment you close the lid, hit a flaky VPN, or just want the agent to keep refactoring while you sleep, the session is gone. LazyAgents pins each agent to a local daemon so the session survives the terminal that started it — reattach from anywhere, reboot the laptop, lose the SSH tunnel, the agent keeps running.

**It's a real scheduler, not a wrapper around `at`.** Crons carry an IANA timezone per entry, handle DST correctly (`docs/adr/0002-cron-dst-fallback-take-first.md`), surface catch-up policy when the daemon was down, and budget-cap runaway runs. Nightly `claude` on the test suite, morning `codex` on the changelog, hourly `opencode` audit — write the cron once and the daemon owns the lifecycle.

**Local-only, no telemetry.** The daemon binds a `0o600` Unix socket (or a SID-restricted Named Pipe on Windows), reads your config from `$XDG_CONFIG_HOME/lazyagents/config.toml`, persists state in a single SQLite file, and never opens a network listener. Each session gets a dedicated git worktree under `$XDG_DATA_HOME/lazyagents/worktrees/`, so an agent gone wrong can never overwrite your main checkout. Production diagnostics — Prometheus-format metrics, JSON structured logs, `lad doctor` health checklist — are documented in [`docs/observability.md`](docs/observability.md).

---

## Where to go next

| You want to… | Read |
|---|---|
| Get LazyAgents on your machine | [`docs/install.md`](docs/install.md) |
| Upgrade an existing install (v1.x → v1.y, or plan for v1 → v2) | [`docs/upgrade.md`](docs/upgrade.md) |
| Wire `lad doctor` + `lad metrics` into your monitoring | [`docs/observability.md`](docs/observability.md) |
| The full user handbook (chapters per feature, EN + 中文) | [`docs/book/`](docs/book/README.md) |
| Architecture, milestones, design rationale | [`report/`](report/) |

## Scope of this release (v1)

LazyAgents v1 is in active development. As of **0.1.1**, the three Sessions-tab paths — sidebar list, `n` New-session, Enter attach to live PTY — are wired end-to-end against the daemon by default (`la --demo` keeps the in-process fixture for screenshots and design iteration). The release pipeline produces binaries for Linux x86_64/aarch64 (gnu + musl), macOS x86_64/aarch64, and Windows x86_64; Linux is the end-to-end-validated target for v1, with the other platforms tracked as release-blockers for GA. Known platform issues are listed in [`docs/book/en/src/troubleshooting.md`](docs/book/en/src/troubleshooting.md).

## License

[MIT](LICENSE).
