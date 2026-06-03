# LazyAgents M3 Chaos Runbook

Scope: Linux v1 validation for the daemon + cron milestone. Run these against a debug or release `lad` built from the same commit under test. The goal is to prove the daemon remains alive, records explicit failures, and avoids scheduler wake-up noise while cron workloads fail or clients disconnect.

## 1. Random child kill

1. Start `lad` with a test shell adapter whose command writes its PID to the project directory and sleeps.
2. Create a cron/session that uses that adapter.
3. Send `SIGKILL` to the child PID, not to `lad`.
4. Expected result: `lad` stays responsive to `initialize`, `sessions.list`, and `events.subscribe`; the affected run/session transitions to a terminal failure state; subsequent cron fires still emit normally.

## 2. SQLite I/O fault injection

1. Start `lad` with `--state-dir` under a temporary directory.
2. After startup, move the SQLite database behind a read-only parent or replace it with a read-only bind/fixture path for the specific fault window. Prefer directory-level read-only injection over chmod-only tests when running as root in CI.
3. Trigger a cron fire and a session write during the fault window.
4. Restore write access.
5. Expected result: storage errors are surfaced/logged, the daemon process does not crash, and a later `sessions.list` / run-history query succeeds after access is restored.

## 3. IPC disconnect / TUI close

1. Connect a client and call `events.subscribe` for `cron_fired`.
2. Close the client socket without sending daemon shutdown.
3. Trigger or publish a later cron fire.
4. Connect a second client and subscribe again.
5. Expected result: the first disconnect does not stop `lad`; the second client receives later `cron.fired` notifications. Automated coverage: `cron_fired_notification_path_survives_tui_disconnect`.

## 4. High-frequency cron + long backoff noise

1. Configure a per-minute cron with exponential failure backoff capped at one hour.
2. Force enough consecutive terminal failures to arm the cap.
3. Observe scheduler fires for at least the first retry window.
4. Expected result: no per-minute events are emitted inside the backoff window, so the admission gate should not log thousand-level `RefuseDeferBackoff` noise. Automated coverage: `high_frequency_cron_long_backoff_does_not_emit_refusal_noise_for_four_hours`.

## 5. Four-hour no-growth soak

1. Start `lad` with a per-minute cron, a failing adapter, and `RUST_LOG=info`.
2. Sample RSS every 60 seconds for four hours with `ps -o rss= -p <lad-pid>`.
3. Keep the workload constant: no increasing number of clients, crons, or active sessions.
4. Pass threshold: after the initial warm-up plateau, RSS delta should remain within 10% or 32 MiB, whichever is larger. Any monotonic growth beyond that requires heap/channel inspection before release.

## 6. Automated checks

Run the Linux CI-sized checks before release:

```sh
cargo test -p la-scheduler --test loop_simulation seven_day
cargo test -p la-scheduler --test loop_simulation high_frequency_cron_long_backoff
cargo test -p la-daemon --test acceptance chaos_
```

These tests cover the seven-day paused scheduler timeline, high-frequency
backoff wake-up noise, external child `SIGKILL`, deterministic SQLite I/O fault
injection, and TUI/IPC disconnect survival. The four-hour RSS soak remains a
manual release check because it is intentionally wall-clock bound.
