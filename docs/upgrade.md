# Upgrading LazyAgents

> Upgrade path for `lad` + `la`. The shape is intentionally simple: **stop the service → replace the binaries → start the service**. Within a major version, the socket and on-disk schema are compatible; across majors, two daemons can coexist long enough for you to drain in-flight sessions.

For first-time install, see [`install.md`](install.md). For production observability (the metric+log surface you should compare across upgrades), see [`observability.md`](observability.md).

## v1.x → v1.y (within the v1 major)

Within the v1 major, every patch and minor release is **wire-compatible** and **schema-compatible**: same `lad-1.sock` socket name, same SQLite migration chain, same config schema (additive only). The upgrade is mechanical.

### Steps

```sh
# 1. Stop the running daemon. Use the verb that matches how you installed it.
#    systemd:
systemctl --user stop lad.service
#    launchd:
launchctl kill SIGTERM gui/$UID/dev.lazyagents.lad
#    Windows Scheduled Task:
schtasks /End /TN \LazyAgents\lad
#    Or, if you ran `lad daemonize` directly:
pkill -INT lad        # SIGINT lets `lad` flush WAL + close the socket cleanly.

# 2. Replace the binaries. Re-run the installer script for your platform.
#    (Same one-liners as `docs/install.md` — they overwrite in place.)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/TinyWhite1997/LazyAgents/releases/latest/download/lazyagents-installer.sh | sh

# 3. Start the service again.
systemctl --user start lad.service
# or: launchctl kickstart -k gui/$UID/dev.lazyagents.lad
# or: schtasks /Run /TN \LazyAgents\lad
# or: lad daemonize

# 4. Verify.
lad --version           # confirms the new build is running.
lad doctor              # exit 0 / exit 2 expected (see install.md).
lad metrics | head      # confirms the metric surface is intact (see observability.md).
```

### Notes

- **Stop first.** The daemon holds an exclusive lock on the SQLite file. Replacing the binary while the old process is still running is fine on Linux (the kernel keeps the old inode alive), but the SQLite lock will block the new process at startup. Always stop the daemon before swapping the binary.
- **Same socket name across v1.x.** The socket path embeds the **protocol major** only — `lad-1.sock`. Every v1.y / v1.z release binds the same path, so existing `la` clients reconnect after the daemon comes back up without any reconfiguration.
- **Schema migrations are forward-additive.** `sqlx migrate` ships a strictly-increasing migration chain (`migrations/00xx__*.sql`). New v1.y releases only **add** columns / tables; old daemons can still read the new schema (architecture §7). You never need to back up the SQLite file before a within-major upgrade, although you can if you want — `~/.local/share/lazyagents/lad.sqlite` (Linux/macOS) or `%LOCALAPPDATA%\lazyagents\lad.sqlite` (Windows).
- **In-flight sessions resume.** Detached sessions (anything you launched as `lad`-owned, not foreground `la`-owned) keep their PTY state in SQLite. After the new daemon starts, `la sessions list` shows them with their last replay buffer; attach with `la attach <session-id>`. Sessions whose underlying backend CLI was a child of the **old** `lad` process will have terminated when you stopped the daemon — they reappear as `interrupted` and you can rerun them.
- **Configuration is additive within v1.x.** New keys land with default values; you do not need to touch your existing `config.toml`. `lad config check` will tell you if anything in the file is rejected by the new build; it will not insist that you opt into new fields.

## v1 → v2 (across majors): multi-version coexistence

Across the v1 → v2 major boundary, the wire protocol may change — that is the explicit license a major bump grants us. To make that safe, **the socket path embeds the protocol major number** (`lad-1.sock` for v1, `lad-2.sock` for v2). That gives us architecture §11.3's "multi-version coexistence" property: a v2 daemon and a v1 daemon can run on the same host, on the same user, at the same time, without conflict.

The intended upgrade path:

1. **Install v2 alongside v1.** v2's binaries (`lad` and `la`) overwrite v1's binaries on disk, but the **v1 daemon process keeps running** because the kernel holds the old inode. Do **not** stop v1 yet.
2. **Bring up the v2 daemon.** Start it (`lad daemonize` or `lad install --service <mode> --start`) on the same machine. It will bind `lad-2.sock`. v1 is still bound to `lad-1.sock`. Both sockets are present, both daemons are healthy. Verify with `lad doctor` (which now reports v2) and the v1-aware `la` client (which keeps talking to `lad-1.sock` until you point it at the new socket).
3. **Drain v1.** Stop scheduling new work on v1 (`la crons list` against `lad-1.sock`, disable each cron). Let in-flight sessions finish — the v1 daemon will close them naturally as the backend CLIs exit.
4. **Retire v1 manually.** Once `la sessions list` against `lad-1.sock` is empty, run the v1 `stop` + `uninstall` verbs against the v1 service (`systemctl --user stop lad.service` was the v1 unit name; v2 may rename it). The v2 daemon is the only one left.
5. **Re-point client.** Switch any wrapper scripts that explicitly target the v1 socket (`LAZYAGENTS_SOCKET=…/lad-1.sock`) to the v2 socket, or unset the override so the client discovers `lad-2.sock` via the standard chain.

This shape is intentionally manual. There is **no auto-migration cron**, no "lad upgrade-major" command. A major bump is a thing you do deliberately, after reading the release notes for v2.

> **Why the manual retire?** The contract that "a v1 daemon and a v2 daemon coexist on the same host" is what guarantees you can roll back: if v2 misbehaves after step 2, you stop the v2 daemon, re-enable the v1 cron entries, and v1 picks up exactly where it left off because nothing about its state changed. Auto-retiring v1 the moment v2 started would forfeit that property.

## Configuration migration

Within v1.x, the config schema is **additive only** — new fields land with defaults, no migration required. Across the v1 → v2 boundary, breaking config changes (field renames, removed fields, semantic changes) will be batched into a single migration documented in the v2 release notes.

The plan, when that day comes, is a dedicated `[migration]` section in `config.toml` plus a `lad config migrate` command:

```toml
# config.toml — example shape under the v2 [migration] section.
# Reserved for v2. v1.x daemons accept the section as a no-op so a v1
# config tree that has been migration-stamped in preparation for v2 does
# not regress when read by the still-running v1 daemon.
[migration]
schema_version = 2          # advertised by the daemon that wrote this file
from_version  = 1           # the schema this file was migrated *from*
migrated_at   = "2026-..."  # set by `lad config migrate`
```

```sh
# Reserved for v2 — not implemented in v1.x.
lad config migrate --from v1 --to v2 --in-place
```

**For v1.x** this is **placeholder documentation only**. There is no `lad config migrate` command shipped in the v1 binary; the `[migration]` section is reserved but unused. The full migration plan, the exact field renames, and the `lad config migrate` flag surface will all land in the v2 release notes — this section exists so that v1 users know what to expect at the v1 → v2 cut.

If you write `[migration]` into your v1 config today, the v1 daemon will reject it on `lad config check` because `deny_unknown_fields` is enforced at the top level. Wait for v2 to ship the schema bump; do not pre-emptively add the section.

## After every upgrade

- **Re-run `lad doctor`.** It is the same checklist as install acceptance (see [`install.md`](install.md#verify-your-install)). A `✓` line that flipped to `!` or `✗` between releases is the single highest-signal indicator that something in your environment changed shape with the new build.
- **Diff the metric surface.** `lad metrics > /tmp/lad-new.prom` and compare to a snapshot from before the upgrade. The A9 metric naming table in [`observability.md`](observability.md#a9-metric-naming-table) is **contract** within v1 — a rename, a missing line, or a type flip is a regression and should be filed as a bug.
- **Spot-check JSON logs.** The default tracing format is JSON with a fixed field schema (see [`observability.md`](observability.md#log-field-schema)). Confirm your log shipper still parses what it parsed before — a silent format change between releases would be a contract break.
- **Re-test your crons.** v1.x is wire-compatible, but cron *behavior* (catch-up policy, throttling, budget enforcement) can be tuned in minor releases. After upgrading, glance at `la crons list` and the next-fire times to confirm nothing surprising changed.
