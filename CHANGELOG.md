# Changelog

All notable changes to LazyAgents are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.1 — 2026-06-06

End-to-end TUI release that closes the v0.1.0 user report ([WEK-92](https://github.com/TinyWhite1997/LazyAgents/issues/92)): the Sessions tab now talks to the daemon by default and the three core paths — list, create, attach — are wired live.

### Fixed

- **Sessions sidebar shows real daemon-backed rows instead of the in-process fixture.** The default `la` invocation connects to `lad` over IPC via the new `RpcSessionSource` and refreshes the sidebar every ~2 s (WEK-93 / WEK-92-A1). The v0.1.0 hardcoded `MockSessionSource::fixture()` rows ("Refactor auth", "Long task", …) are gone from production builds.
- **`n` opens a real New-session modal and Confirm actually creates a session.** The `SessionSource` trait gained `create_session(NewSessionRequest)`; the modal collects backend / prompt / worktree, `Ctrl+Enter` (or Enter on a non-Prompt field) calls `sessions.create` on the daemon, errors round-trip as toasts, and the freshly minted session appears on the sidebar on the next tick (WEK-94 / WEK-92-A2).
- **Enter attaches to the live PTY.** `Selection::Session` now spawns an `attach_pump` that owns its own daemon connection, decodes `session.output` with resume cursors, and renders bytes into the `Transcript` widget. Keystrokes route through the runner's input-owned pane to `sessions.write`; `Ctrl+B d` detaches and returns focus to the sidebar (WEK-96 / WEK-92-A3).
- **`RpcSessionSource` background refreshes ferry into the TUI sidebar.** A push channel from the bg poller into the runner triggers `App::refresh_sessions` on each successful `sessions.list`, so daemon-side mutations (CLI create, cron-fired sessions, IPC-driven imports) appear without user input. The `Modal::NewSession::Confirm → sessions.create → snapshot` race is closed: the bg loop now awaits `refresh_cache` before replying to the TUI thread (WEK-98 / WEK-92-A4.1).

### Added

- **`la --demo` keeps the WEK-26 in-process fixture available** for regression screenshots, design iteration, and CI smoke runs that can't bind a real `lad`. Production `la` never injects fixture data into a real workspace.

### Notes

- No wire-protocol changes — `lad` 0.1.0 and `la` 0.1.1 interoperate. Clients written against the JSON-RPC surface in `docs/schema/` need no migration.
- The `crons.*` TUI binding still goes through `MockCronSource`; live cron round-trip is tracked separately under M3.5.

## 0.1.0 — 2026-06-06

Initial GA tag-cut. Daemon (`lad`) — sessions, crons, worktrees, adapters — fully functional over IPC; release pipeline produces Linux x86_64/aarch64 (gnu + musl), macOS x86_64/aarch64, and Windows x86_64 binaries via cargo-dist. The TUI shipped with a daemon-backed status bar but mock-backed Sessions sidebar (the regression fixed in 0.1.1).
