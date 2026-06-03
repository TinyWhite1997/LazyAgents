# Crons

LazyAgents has a real cron scheduler. You give it a cron expression, an IANA timezone, a backend, and a prompt — it spawns a fresh session each time the schedule fires. Useful for nightly refactors, morning code review summaries, hourly link checkers — anything you'd otherwise rig up with `systemd-timer` and a shell wrapper.

## Cron grammar

LazyAgents accepts **5-field** and **6-field** cron expressions.

| Format | Fields | Example |
|---|---|---|
| 5-field (classic) | `minute hour day-of-month month day-of-week` | `0 9 * * 1-5` (weekdays at 09:00) |
| 6-field (with seconds) | `second minute hour day-of-month month day-of-week` | `*/30 * * * * *` (every 30 seconds) |

5-field input is auto-promoted to 6-field by prepending `0`, so a classic expression fires at second `:00` of the matched minute.

7-field (with year) input is **rejected** — a deliberate choice to avoid silent surprises.

Parsing is provided by the [`cron`](https://crates.io/crates/cron) crate (version 0.16); refer to that crate's documentation for the full token grammar (`*/N`, ranges, lists, etc.).

## Timezone and DST

Every cron stores an IANA timezone name (e.g. `America/Los_Angeles`, `Asia/Shanghai`, `UTC`). Scheduling arithmetic happens in that zone via `chrono-tz`; the scheduler stores fire times in UTC internally and renders them back in the cron's own zone for display.

**Fall-back (ambiguous) wall times:** LazyAgents applies a **take-first** policy. When the local wall-clock occurs twice on a DST fall-back day (e.g. `01:30 America/Los_Angeles` on Nov 1 maps to both `08:30Z` and `09:30Z`), LazyAgents fires only the first instance. This matches the common user expectation that a once-daily wall-clock cron fires once per day, and avoids duplicate spend or duplicate side effects.

**Spring-forward (nonexistent) wall times:** these are skipped using the default `chrono-tz` / `cron` behaviour; the next valid wall time is used.

The full rationale is in [ADR-0002](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/adr/0002-cron-dst-fallback-take-first.md).

## Create a cron

> **v1 status.** The daemon-side cron surface (`crons.upsert`, `crons.set_enabled`, `crons.run_now`, `crons.dry_run`, the admission gate, scheduler, catch-up, archiving) is fully wired in v1 — every behaviour described below is what `lad` actually does once a cron lands in SQLite. The **TUI Crons tab is still mock-backed** (tracked as M3.5 in the source): the editor, list, and `Space` / `r` / `R` / `d` keys all manipulate an in-memory `MockCronSource` and **do not yet round-trip to the daemon**. To create, enable, or trigger a real cron today, drive `crons.*` over the IPC socket; the TUI binding tables below describe the M3.5 wiring already drafted in the UI layer.

### v1 path: JSON-RPC

```json
{"jsonrpc":"2.0","id":1,"method":"crons.upsert","params":{
  "name":      "nightly-refactor",
  "project_id":"<your project id>",
  "backend":    "claude",
  "spawn_args": [],
  "prompt":    "Run the nightly refactor checklist.",
  "cron_expr": "0 2 * * *",
  "tz":        "America/Los_Angeles"
}}
```

Then enable it with `crons.set_enabled { cron_id, enabled: true }`. See [Enabling a cron](#enabling-a-cron) below — and note that v1's enable path has no token gate, no auto-disable on sensitive edits, and no IPC-level prompt size cap.

### TUI editor (M3.5 wiring, today mock-backed)

In the TUI you switch to the **Crons** tab and press **`n`** to open the editor. Cycle fields with `Tab`:

| Field | Notes |
|---|---|
| Name | Human label. |
| Backend | `claude`, `codex`, `opencode`, ... |
| Spawn args | Extra args passed to the backend CLI. |
| Cron expr | 5 or 6 fields. |
| Tz | IANA name. |
| Prompt | Don't put credentials here — see [Security caveats](#security-caveats). A 64 KiB cap (`MAX_PROMPT_BYTES`) exists in `cron_security` but is not enforced at the v1 IPC boundary yet. |
| Budget | Daily USD cap, runtime per run, max concurrent runs, etc. |

`Ctrl+S` saves the draft. `Esc` discards. **In v1 these writes land in the TUI's mock source only**; M3.5 swaps in a live `IpcCronSource` and the same keystrokes will round-trip to `crons.upsert`.

## Crons-tab keys

| Key | Effect (today — against the mock) | Effect after M3.5 wires the live source |
|---|---|---|
| `j` / `k` / `↓` / `↑` | Move cursor | Same |
| `n` | New cron draft | Same; save will call `crons.upsert` |
| `e` / `i` / `Enter` | Edit the highlighted cron | Same |
| `Space` | Toggle local enabled flag | `crons.set_enabled` (single call in v1; token-gated when the security helper is wired through) |
| `r` | Mock `trigger_now` | `crons.run_now` |
| `R` | Local dry-run preview | `crons.dry_run` |
| `d` | Delete from mock | `crons.delete` (with confirmation modal) |
| `Ctrl+S` | Save the current draft | Same; round-trips to `crons.upsert` |
| `Esc` | Cancel the draft | Same |

Until M3.5 lands, **don't rely on the TUI to schedule real work** — anything you do here is local-only and disappears when you quit `la`.

## Enabling a cron

Crons don't become enabled on save — `crons.upsert` always lands them disabled. To enable one in v1, send a single `crons.set_enabled`:

```json
{"jsonrpc":"2.0","id":3,"method":"crons.set_enabled","params":{
  "cron_id": "<the id returned by upsert>",
  "enabled": true
}}
```

The wire params are just `{cron_id, enabled}`; the response carries the updated cron row. There is **no enable-time hardening on the v1 RPC surface yet**: no confirmation token, no auto-disable on sensitive edits, no IPC-level prompt size cap. The daemon ships a `cron_security` module (`crates/la-daemon/src/cron_security.rs`) with a 5-minute single-use confirmation token + summary helper, a `SENSITIVE_CRON_FIELDS` allowlist (backend, args, prompt, schedule, timezone, runtime limits, max-per-day, daily budget), and a 64 KiB `MAX_PROMPT_BYTES` cap — but **the dispatcher and scheduler don't call any of it today**. Treat all of those as planned hardening, not current user-facing behaviour.

What v1 *does* enforce today:

- **New cron defaults to disabled.** `crons.upsert` for a brand-new id sets `enabled = false`; you must explicitly `crons.set_enabled` to turn it on.
- **Updates preserve the current `enabled` bit.** Updating an already-enabled cron — even when changing backend, prompt, or schedule — leaves it enabled and the next scheduled fire will use the new values. There is no auto-disable safety net in v1; if you're scripting risky edits, call `set_enabled { enabled: false }` first.

Once `cron_security` is wired to the RPC surface, both the auto-disable on sensitive edits and the token round-trip will kick in. Until then the only protection on `crons.set_enabled` is "you have to call it explicitly".

## What fires when the cron triggers

The scheduler heap pops the next entry, the executor acquires an admission lock, evaluates per-cron and global quotas (max concurrent, max-per-day, budget), inserts a `runs` row, and then:

1. Resolves the adapter for `backend`.
2. Resolves the project root.
3. Calls `SessionManager::spawn` with `spawn_args`, the resolved cwd, and the prompt pre-loaded.
4. Attaches the new `session_id` to the `runs` row, sets `status = running`.
5. Publishes a `cron.fired` notification (the TUI flashes a status-bar pulse).
6. Spawns a per-second watcher that enforces `max_runtime_s` (sends SIGTERM on timeout), bumps `consecutive_failures` on failure, and auto-pauses the cron once `consecutive_failures >= pause_on_consecutive_failures`.

Cron sessions are **non-interactive** by default — input ownership requires you to explicitly attach (`Enter` on the session row). This is intentional: a cron is unattended workflow, not a live shell.

## Catch-up policies

If the daemon is asleep when a fire was due (laptop suspended, daemon crashed, system rebooted), the per-cron `catchup_mode` decides what happens on wake:

| Mode | Behaviour |
|---|---|
| `skip` | Missed fires are dropped silently. |
| `coalesce` (default) | A single fire is emitted for the entire missed window. |
| `replay` | Every missed fire is enqueued and run in order. **Use with care** — a per-minute cron that's been down for a day will try to run 1440 times. |

For ambiguous wall times during DST fall-back, missed-fire enumeration also respects the take-first policy, so `coalesce` and `replay` don't accidentally back-fill duplicate executions for the overlap hour.

## Failure handling

- `failure_backoff` (default `expo(1m,2,1h)`): exponential 1m → 2m → 4m → ... capped at 1h. Per-minute crons that fail in a row stop emitting per-minute wake-ups inside the backoff window — verified by the four-hour scheduler test.
- `pause_on_consecutive_failures` (default 5): after N consecutive terminal failures, the cron is auto-paused. The TUI shows it as disabled with a "paused after N failures" badge.
- `consecutive_failures` resets to 0 on the first successful run.

## Where state lives

All cron state — definitions, catch-up watermarks, run history — lives in the SQLite database `lad.sqlite`. There is no separate cron config file.

| Table | Contents |
|---|---|
| `crons` | One row per cron definition: schedule, project, backend, prompt, args, budget, `consecutive_failures`, `last_fired_at`, `next_fire_at`. |
| `runs` | One row per fire: `scheduled_at`, `started_at`, `finished_at`, status, exit code, cost, error. |

The scheduler's in-memory heap is re-seeded from `crons` on every daemon start.

Old `runs` rows are pruned after a retention window (default 90 days, swept once a day at local 03:17). Before deletion they're appended to monthly `<state_dir>/runs/archive/<yyyymm>.jsonl.zst` files (zstd-compressed JSONL), so the history is grep-able without touching the live database.

## Security caveats

- **Prompts are stored as plaintext.** They're not encrypted or treated as secrets. Don't put credentials in a prompt.
- **Cron-spawned commands are spawned directly** — the executor does not wrap them in `/bin/sh -c`. Shell-injection through `spawn_args` is not a thing.
- **A 64 KiB prompt cap exists in `cron_security` (`MAX_PROMPT_BYTES`) but is not enforced at the v1 RPC boundary**; the daemon will accept and store arbitrarily large prompts today. Treat 64 KiB as the design target you should respect anyway — once `cron_security` is wired through, oversize prompts will be rejected before the scheduler sees them.

## Dry-run before you enable

Call `crons.dry_run` with `count: N` (up to 20) to see the next N fire times in the cron's own timezone. This is the cheapest way to catch a `0 9 * * 7` (Sunday at 09:00) when you meant `0 9 * * 1` (Monday). The TUI's `R` key drives a local preview today; after M3.5 it will round-trip to the daemon.

## See it without enabling

`crons.run_now` bypasses the schedule and fires the cron once immediately, going through the same admission gate as a scheduled fire (so quotas still apply). Useful for sanity-checking a fresh cron before you flip it on. The TUI's `r` key will drive this once M3.5 lands; for v1 invoke the RPC directly.
