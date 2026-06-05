# Observability (M4.5 / WEK-75)

Single source of truth for LazyAgents' metric naming table, log field schema,
and recommended external collectors. Pairs with architecture §9.3 and the
WEK-75 brief; future metric additions go through an ADR plus a row in the
table below.

The `lad_*` prefix is contract (Rev2 R4): every metric the daemon emits keeps
it. Adding or renaming a metric without updating both this file and
`la_observ::describe_metrics()` is a wire break.

## Scrape surface

`lad metrics` dials the daemon's main socket (Unix UDS / Windows Named Pipe)
and issues a single `metrics.scrape` JSON-RPC call. The response carries the
Prometheus text-exposition body in `result.body` and the CLI prints it to
stdout unchanged — no prefix, no trim, no rewrite. There is no separate
metrics socket; the same security boundary as every other RPC applies
(`0o600` UDS / SID-restricted pipe).

> Note on "byte-identical": the CLI is byte-identical with respect to the
> RPC body of its own scrape — the bytes it prints are exactly the bytes
> the daemon returned. Two scrapes back-to-back are NOT byte-identical:
> every `metrics.scrape` call itself bumps `lad_rpc_requests_total{
> method="metrics.scrape"}` and the scheduler-health loop ticks the gauges
> in the background, so metric VALUES drift between scrapes. The
> acceptance test
> `metrics_scrape_rpc_and_cli_expose_same_a9_surface` pins the pieces that
> the CLI is responsible for: same `# TYPE` / `# HELP` preamble, same
> metric NAME set, structural shape unchanged.

```text
# Inside the box
$ lad metrics | head
# HELP lad_rpc_requests_total Total JSON-RPC requests handled by lad, ...
# TYPE lad_rpc_requests_total counter
...

# From a sidecar / Prometheus scrape job
$ lad metrics > /tmp/lad.prom    # then ingest via node_exporter textfile, etc.
```

The acceptance test at `crates/la-daemon/tests/acceptance.rs ::
metrics_scrape_rpc_and_cli_expose_same_a9_surface` pins five properties:
(1) the RPC body has the `# TYPE` / `# HELP` preamble shape,
(2) every entry in the A9 naming table appears in a `# TYPE` line of the
body (silent drop of a `describe_*!` call trips the test),
(3) every A9 metric the test can drive in-process
(`lad_rpc_requests_total`, `lad_rpc_duration_seconds`, `lad_session_active`,
`lad_scheduler_queue_depth`) has at least one sample line
(`describe`-without-`emit` drift trips the test),
(4) `# TYPE` / `# HELP` preamble shape matches between CLI and RPC,
(5) every metric name present in one body is present in the other.

## A9 metric naming table

Pinned contract. Additions go through an ADR; renames are wire breaks.

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `lad_rpc_requests_total` | counter | `method`, `result` | RPC call counter; `result=ok|error`. |
| `lad_rpc_duration_seconds` | histogram | `method` | Per-RPC handler latency. No `result` label — keeps cardinality bounded. |
| `lad_session_active` | gauge | `backend` | Currently active sessions known to the daemon. |
| `lad_session_output_bytes_total` | counter | `backend` | Session output bytes delivered to attached clients. |
| `lad_cron_runs_total` | counter | `status` | Cron run **terminal-status** counter. Raw `runs.status` values (`completed` / `failed` / `timed_out` / `cancelled` / `budget_exceeded`) are mapped to the four canonical contract values at the emit boundary (`la_storage::repos::cron_status_label`): `ok` / `error` / `timeout` / `budget_rejected`. The start pulse is intentionally NOT counted here. |
| `lad_cron_missed_total` | counter | `mode` | Catch-up disposition (`skip` / `coalesce` / `replay`). |
| `lad_cron_throttled_seconds_total` | counter | — | Cumulative seconds the scheduler delayed a fire due to loadavg throttle. |
| `lad_pty_spawn_duration_seconds` | histogram | `backend` | PTY/session spawn duration. |
| `lad_storage_write_latency_seconds` | histogram | `op` | SQLite write latency by operation (`runs.create`, `runs.finish`, `chunks.append`, ...). |
| `lad_runs_archive_pruned_total` | counter | — | Rows the daily runs-archive job deletes from SQLite. |
| `lad_scheduler_queue_depth` | gauge | — | Global count of cron fires waiting in the scheduler heap. |
| `lad_scheduler_clock_skew_seconds` | gauge | — | Most recently observed wall-clock jump (signed seconds). |
| `lad_adapter_drift_total` | counter | `backend` | Adapter protocol-drift events (replaces the M3 `tracing::error!(target="adapter_drift")` as the canonical surface; the log event is preserved for Loki/Grafana log ingest). |

## Log field schema

The default tracing format is JSON (`tracing-subscriber::fmt().json()` with
the configuration in `la_observ::init_json_tracing`). Every line carries the
fixed field set below; per-event fields nest under the top-level keys via
`flatten_event(true)`.

| Field | Type | Notes |
| --- | --- | --- |
| `timestamp` | string (RFC3339) | UTC. |
| `level` | string | `trace` / `debug` / `info` / `warn` / `error`. |
| `target` | string | The tracing target (typically `<crate>::<module>` or `adapter_drift` for drift events). |
| `message` | string | The unstructured message portion of the event. |
| `fields.*` | object | Event-specific fields. Includes `trace_id` for any event emitted inside an RPC or cron-fire span. |
| `span.*` | object | Tracing span hierarchy (`with_current_span(true)` / `with_span_list(true)`). |

`trace_id` is a 128-bit hex string minted at:

- the entry of every JSON-RPC request handler (`la_daemon::dispatcher::handle_request`),
- the entry of every cron fire (`la_daemon::scheduler::process_fire`), then
  threaded through admit → spawn → watcher → `runs().finish()`.

Both call sites use `la_observ::new_trace_id()`. The same value is injected
into the surrounding `tracing::Span` so every downstream event inherits it
via the `span.trace_id` field. For cron runs the trace_id is also written as
the first header line of `runs.tail_log` (`# trace_id=<id>\n`) so a future
`lad runs get` (or a direct row query) can pivot straight into the Loki
window for that fire without a separate join.

### `--log-format compact`

Developers running `lad start --log-format compact` (or
`LAZYAGENTS_LOG_FORMAT=compact lad start`) get the
`tracing-subscriber::fmt::layer().compact()` formatter on stderr instead.
The compact path keeps the same recent-event ring (used by the crash
reporter) so a panic during local development still produces a usable crash
JSON. JSON is the production default; compact must not be used in CI or in
service-managed deploys.

## Recommended external collectors

The daemon ships no embedded scraper / log shipper. Recommended pairings:

- **Prometheus**. Run `lad metrics` periodically (`node_exporter`'s textfile
  collector, a cron job, or a sidecar that `lad metrics > /tmp/lad.prom`).
  The exposition body parses with the standard Prometheus / OpenMetrics
  ingestion path; the `lad_*` prefix is unique to LazyAgents so scrape
  pipelines never collide with host metrics.
- **Grafana / Loki**. Pipe the daemon's stderr (JSON tracing) through Promtail
  or Vector with `decoded.format = json`. Both fields `trace_id` and
  `target = "adapter_drift"` are reliable filter keys for dashboards.
- **Crash reports**. `la_observ::install_crash_reporter` writes a JSON file
  per panic to `<state_dir>/crashes/`. The format embeds the last 100
  tracing events (with their `trace_id`s) so a post-incident dashboard can
  correlate the crash with the surrounding requests.

## See also

- `crates/la-observ/src/lib.rs` — recorder + tracing installers, recent-event
  ring, crash reporter.
- `crates/la-daemon/src/dispatcher.rs ::handle_request` — per-RPC trace-id
  span + the two pinned RPC-level metrics.
- `crates/la-daemon/src/health.rs` — adapter-drift counter + drift log event.
- Architecture doc §9.3 — the original metric naming + observability rules.
