# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

LazyAgents is a local daemon (`lad`) plus a ratatui TUI (`la`) that keeps unattended coding-agent sessions (Claude Code, OpenAI Codex, sst.dev OpenCode) alive across terminal disconnects, reboots, and cron-triggered runs. Each session gets a dedicated git worktree. The daemon is local-only — Unix domain socket (`0o600`) or Windows Named Pipe with an owner-only DACL; no network listener, no telemetry. State lives in a single SQLite file.

The toolchain is pinned via `rust-toolchain.toml` (currently `1.96.0`); CI uses `dtolnay/rust-toolchain@1.96.0`.

## Commands

Build / install from source:

```sh
cargo install --path crates/la-daemon --locked   # installs `lad`
cargo install --path crates/la-tui    --locked   # installs `la`
```

Workspace-wide checks (mirror `.github/workflows/ci.yml`):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --lib --bins -- -D warnings
cargo test --workspace --all-targets
```

Single-crate or single-test workflows:

```sh
cargo test -p la-core                        # one crate
cargo test -p la-daemon lifecycle::          # one test module
cargo test -p la-scheduler dst_fallback      # one test by name substring
```

Wire-schema golden check (must be re-run after **any** `la-proto` type change; CI fails otherwise):

```sh
cargo run -p la-proto --bin la-proto-gen-schema -- docs/schema
```

Config / install dry-runs the CI also runs (good for local smoke):

```sh
cargo run -p la-daemon --bin lad -- config check --config crates/la-daemon/templates/config.example.toml
cargo run -p la-daemon --bin lad -- install --service systemd --dry-run
cargo run -p la-daemon --bin lad -- install --service launchd --dry-run        # macOS
cargo run -p la-daemon --bin lad -- install --service windows-task --dry-run   # Windows
```

User handbook (mdBook, EN + zh-CN sources in `docs/book/{en,zh-CN}/`):

```sh
mdbook build docs/book/en
mdbook serve --open docs/book/en
```

Release pipeline is `cargo-dist` driven; `cargo-dist-version = "0.32.0"` is **pinned** in `Cargo.toml`'s `[workspace.metadata.dist]`. Don't bump without also updating the installer URLs in `.github/workflows/release.yml`. The `release.yml` workflow is hand-curated (`allow-dirty = ["ci"]`); do not let `dist init` regenerate it.

## Architecture

The workspace is a layered split — each crate has a one-job remit and the dependency direction is strictly downward. Read these top-of-file `//!` docs first when touching a crate; they carry the architecture invariants.

Wire / transport layer (no business logic):

- `la-proto` — JSON-RPC 2.0 envelope + method/notification surface. No transport deps. JSON Schema is generated from these types into `docs/schema/`; the golden diff in CI gates changes.
- `la-ipc` — 4-byte big-endian length-prefix codec (4 MiB cap), cross-platform UDS / Named Pipe transport, version-negotiating handshake, `OutputHub` multi-attach fan-out, `Connection` sink/stream. Windows Named Pipe applies an owner-only DACL and verifies the peer SID.

Adapter + PTY layer:

- `la-pty` — `portable_pty` wrapper with a tokio-friendly `PtyChild` (mpsc reader, writer, resize, signal, wait).
- `la-adapter` — pure `AgentAdapter` trait + `ProbeResult`. Adapters never touch IPC, SQLite, or own a PTY — they just describe how to spawn a backend and parse its probe output. One file per backend (`claude.rs`, `codex.rs`, `opencode.rs`).

Persistence + scheduling + config + observability:

- `la-storage` — SQLite schema, sqlx migrations under `migrations/`, WAL pool setup, typed repos. `CURRENT_SCHEMA_VERSION` lives in `lib.rs` and must be bumped together with a new migration.
- `la-scheduler` — IANA-tz cron engine (ADR-003 / arch §5). `tokio::time::sleep_until` heap, three catch-up policies (skip / coalesce / replay), 60 s clock-skew detector. The §5.4 backoff state is an **in-memory mirror** — the daemon's run-executor must re-seed it via `SchedulerHandle::update_backoff_state` after every `upsert` whose SQLite row has `consecutive_failures > 0`, otherwise restart silently disables the heap floor. ADR-002 (`docs/adr/0002-cron-dst-fallback-take-first.md`) defines DST behavior.
- `la-config` — TOML schema with `deny_unknown_fields`, four-tier load precedence (CLI flag > env > file > built-in default) exposed as `Resolved<T>` with source attribution, and the cross-platform `resolve_config_path()` chain. Owned outside `la-daemon` so `la-tui` can read its `[ui]` table.
- `la-observ` — `tracing-subscriber` + `metrics` plumbing, recent-events ring, structured-JSON log layer.

Composition layer:

- `la-core` — owns `SessionManager` (spawn / attach / detach / write / signal / orphan cleanup) and the global `EventBus`. Critical invariants pinned by tests: `la` never holds a PTY directly (§1.2), single-writer enforcement (§3), output ring buffer is delegated to `OutputHub` (§6.2 — 2 MiB/session, gap on overflow), client disconnect never reaps the child.
- `la-daemon` — assembles `la-core` + `la-storage` + `la-ipc` into the `lad` binary. Owns CLI subcommands (`config`, `doctor`, `install`, `daemonize`), the dispatcher / adapter registry, scheduler wiring, signal handling, health probes, and service-template rendering. The library surface (`Daemon`, `DaemonConfig`) lets integration tests drive a daemon in-process without going through the CLI. Socket paths embed the protocol major (`lad-1.sock`) for forward-compat (§11.3).
- `la-tui` — `la` binary. **Depends only on `la-proto` + `la-ipc`** — never reaches into `la-core` or `la-storage`. The Crons-tab cron preview uses the workspace-pinned `cron` + `chrono-tz` versions so the editor preview is byte-identical to what the daemon will schedule.

Integration crates under `integration/` (`m0-smoke`, `m2-smoke`, `m4-macos-smoke`) hold platform-conditional end-to-end tests. `m4-macos-smoke` `cfg`-gates every test on `target_os = "macos"` so it compiles as a no-op on other runners.

## Conventions to honor when editing

- **Any `la-proto` type change requires regenerating `docs/schema/`.** CI's `gen-schema --check` is a golden diff.
- **Don't widen `la-tui`'s dependencies.** Architecture §2.1 keeps the TUI on `la-proto` + `la-ipc` only.
- **Service-unit templates are the source of truth.** `crates/la-daemon/templates/{lad.service,dev.lazyagents.lad.plist,lad-task.xml}` are hand-written for hardening (`UMask=0077`, `RuntimeDirectoryMode=0700`, plist `RunAtLoad=false`, Scheduled Task `RunLevel=LeastPrivilege`). Don't switch to cargo-dist's auto-generators. The same files are also `include`d in `[workspace.metadata.dist]` so they ship in the release archive — keep both lists in sync.
- **Don't pin `*-latest` runners** in any workflow under `.github/workflows/`. The PR-level CI matrix and release matrix share the same runner-pin table (see comment block at the top of `ci.yml`).
- **Cron failure-backoff** has to be re-armed in memory after restart — see the `la-scheduler` §5.4 block above.
- **User docs ship in the same PR as the feature.** If a change touches a user-visible RPC method, TUI keystroke, config key, env var, file path, or CLI flag, update `docs/book/en/src/` (and ideally `docs/book/zh-CN/src/`). See `docs/book/README.md` chapter-responsibility table.

## Where deeper context lives

- `report/技术架构设计.md` — section numbers cited throughout (`§1.2`, `§3`, `§5.4`, `§6.2`, `§11.3` …) point here.
- `report/产品设计文档.md` — PRD; section refs like "PRD §5.6 渐进披露" come from this.
- `docs/adr/` — Architectural Decision Records (currently main-branch protection and the cron DST fallback).
- `docs/{install,upgrade,observability,security,chaos,data-ownership}.md` — ops-facing references.
- The top-of-file `//!` docs on every crate's `lib.rs` are kept current; prefer them over duplicating the same prose elsewhere.
