# ADR 0001: Cron DST fall-back take-first semantics

## Status

Accepted

## Context

LazyAgents cron expressions are evaluated in IANA timezones instead of fixed
UTC offsets. That preserves wall-clock behavior across daylight saving time
changes, but fall-back transitions create an ambiguous hour: the same local wall
time occurs twice with two different offsets.

The underlying `cron` iterator can surface both instants. For example,
`30 1 * * *` in `America/Los_Angeles` on 2026-11-01 can produce both
01:30 PDT and 01:30 PST. That is faithful to the timezone database, but it can
also double-run daily jobs whose owners usually think in local civil time.

## Decision

LazyAgents keeps IANA timezone evaluation and adds a product-level take-first
bias for fall-back overlaps. If a local wall time maps to two instants, the
scheduler treats the earlier instant as the cron fire and suppresses the later
instant.

Spring-forward gaps continue to follow IANA behavior: a nonexistent local wall
time does not fire.

## Tradeoffs

Double-trigger semantics maximize fidelity to the raw timezone iterator and
avoid dropping any instant that matches the wall-clock pattern. They are useful
for low-level calendaring systems, but they surprise automation users because a
daily cron can run twice on one day.

Take-first semantics make every local wall timestamp fire at most once. They
avoid duplicate daemon actions and make dry-run output easier to explain. The
cost is that sub-hourly crons suppress the repeated fall-back hour's second
pass; for example, `* 1 * * *` emits the first 01:00-01:59 sequence and skips
the repeated 01:00-01:59 sequence.

## Affected IPC surfaces

- `crons.dry_run`: preview output lists only the first occurrence of any
  fall-back overlapping wall time.
- Daemon catch-up via `fires_between`: missed-fire replay uses the same
  take-first filter, so restart recovery does not enqueue suppressed second
  occurrences.
- Daily/live scheduling via `next_after`: heap rescheduling skips the second
  occurrence even when the previous fire was the first occurrence in the
  overlap.

## Consequences

All public `CronSpec` enumeration APIs share the same rule. Operators get a
single visible contract across dry-run, live execution, and catch-up: cron
matching is IANA wall-clock based, with a LazyAgents take-first bias during the
fall-back overlap.
