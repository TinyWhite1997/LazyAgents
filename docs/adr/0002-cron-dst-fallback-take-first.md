# ADR-0002: Cron DST fallback take-first semantics

## Status

Accepted

## Context

LazyAgents cron definitions store an IANA timezone name and use `chrono-tz` with
the workspace-pinned `cron` crate. This keeps schedules aligned with IANA DST
rules instead of fixed UTC offsets. During a fall-back transition, however, some
local wall-clock times occur twice. For example, `01:30 America/Los_Angeles` on
2026-11-01 maps to both `08:30Z` and `09:30Z`.

The underlying cron iterator can surface both instants. That is technically
faithful to IANA rules, but it creates user-visible double execution for a cron
that visually appears to be scheduled once.

## Decision

LazyAgents keeps IANA timezone resolution, then applies a take-first bias for
ambiguous fall-back wall times:

- If a scheduled local wall time is unambiguous, emit it normally.
- If a scheduled local wall time is ambiguous during the fall-back overlap, emit
  only the first occurrence and skip the repeated second occurrence.
- If a scheduled local wall time is nonexistent during a spring-forward gap, keep
  the existing `chrono-tz` / `cron` behavior.

## Alternatives considered

| Option | Pros | Cons |
| --- | --- | --- |
| Double-trigger both occurrences | Fully exposes all IANA-mapped instants; no LazyAgents-specific filtering | Surprises users with duplicate daily jobs, duplicate spend, and duplicate side effects in the same displayed hour |
| Take-first single trigger | Matches the common user expectation that a daily wall-clock cron fires once per day; minimizes duplicate cost and side effects | Adds a LazyAgents policy layer on top of the cron iterator and must be documented consistently |
| Take-second single trigger | Also avoids duplicates | Delays the job relative to the first visible wall-clock occurrence and is harder to explain |

Take-first is the least surprising user contract for unattended agent runs where
duplicate execution can spend money, mutate worktrees, or send repeated
notifications.

## Affected surfaces

- `crons.dry_run`: preview output lists only the first occurrence of an ambiguous
  fall-back wall time. The TUI mirrors this rule so its preview stays consistent
  with daemon scheduling.
- Daemon catch-up via `CronSpec::fires_between`: missed-fire enumeration skips
  the repeated second occurrence, so `skip`, `coalesce`, and `replay` policies do
  not back-fill duplicate executions for the overlap hour.
- Daily scheduling via `CronSpec::next_after`: regular heap rescheduling skips
  the repeated second occurrence and advances to the next eligible wall-clock
  fire.

## Consequences

Cron behavior remains DST-aware, but "follow IANA rules" now means "resolve with
IANA timezone data, then apply LazyAgents' take-first policy for ambiguous
fall-back hours." Tests cover `next_after`, `fires_between`, and `preview` around
the 2026 America/Los_Angeles fall-back transition to prevent accidental
regression to double-trigger behavior.
