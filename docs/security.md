# LazyAgents Security Hardening

## Threat Model

LazyAgents v1 exposes no network listener by default. The main local attack
surfaces are same-host IPC abuse, unattended cron execution, shell injection
through spawned agent commands, and credential leakage through logs or cron
metadata.

## IPC Controls

- Unix daemon sockets are created under a 0700 runtime directory.
- Unix listener bind temporarily uses `umask(0077)`, then chmods the socket to
  0600.
- Unix accept verifies `SO_PEERCRED.uid == geteuid()` before JSON-RPC
  handshake. A peer that reaches the socket through stale permissions is still
  rejected if it is not the daemon owner.
- Windows Named Pipe code uses `reject_remote_clients(true)`. v1 validation is
  Linux-only; current-user SID ACL hardening remains a Windows follow-up.
- The daemon does not enable TCP listening by default.

## Cron Enablement Controls

- New or edited cron definitions must not become enabled through a single
  upsert call.
- Enabling a cron is a two-step backend decision: first call returns a
  confirmation token and summary, second call must present the token.
- Confirmation tokens are 128-bit random, in-memory only, expire after 5
  minutes, and are single-use. Successful enable deletes the token. Invalid,
  expired, or cross-cron tokens do not enable anything.
- Sensitive cron fields are allowlisted. Changing project, backend, spawn
  arguments, prompt, schedule, timezone, runtime limits, run/day limits, cost
  budget, or auto-pause threshold auto-disables an enabled cron and invalidates
  pending tokens. `failure_backoff` is intentionally not sensitive because it
  changes retry cadence after failures, not what the cron does or spends.
- Prompt payloads are capped at 64 KiB at the backend boundary.

## Secrets And Logging

- Secret-bearing values use `SecretString` and render as `***` for `Debug` and
  `Display`.
- LazyAgents does not store backend credentials. Authentication remains owned
  by the installed agent CLIs.
- Cron metadata may include prompt text; users should treat prompts as stored
  local data and avoid placing credentials in them.

## Spawn Controls

- Cron-triggered sessions must use direct executable spawning. Do not wrap
  cron commands in `/bin/sh -c`.
- Cron-triggered sessions are non-interactive by default; input ownership
  requires an explicit later user action.
