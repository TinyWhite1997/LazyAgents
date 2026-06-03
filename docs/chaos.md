# Chaos runbook

This runbook covers the Linux-only M3.7 chaos checks for `lad`. Run from the
repository root on `ubuntu-latest` or a local Linux host.

## Automated regression suite

```sh
cargo test -p la-scheduler --test loop_simulation
cargo test -p la-daemon --test acceptance chaos_
```

Coverage:

- Seven-day `tokio::time::pause` scheduler timelines for `skip`, `coalesce`,
  `replay`, and over-cap degradation.
- External child death: a session child is killed with `SIGKILL`; the daemon
  stays responsive and the session settles to `exited`.
- IPC/TUI disconnect: dropping the client connection does not reap the
  daemon-owned session, and a later client still receives `cron.fired`.

## Manual SQLite I/O fault injection

Use a disposable state directory. Do not run this against a real user profile.

```sh
tmp="$(mktemp -d)"
runtime="$tmp/runtime"
state="$tmp/state"
project="$tmp/project"
mkdir -p "$runtime" "$state" "$project"

cargo build -p la-daemon --bin lad
target/debug/lad start \
  --socket "$runtime/lad-1.sock" \
  --state-dir "$state" \
  --test-shell-adapter "for i in \$(seq 1 600); do printf 'chaos-%04d\n' \$i; sleep 0.05; done" &
lad_pid=$!
```

Create and attach a session with the CLI/client under test, then inject I/O
errors by removing write access from the SQLite directory:

```sh
chmod a-w "$state"
sleep 20
chmod u+w "$state"
```

Expected result:

- `lad` process remains alive.
- Existing IPC clients may see explicit RPC errors while SQLite is unwritable;
  the process must not panic or exit.
- After restoring write access, `sessions.list` responds and daemon shutdown
  completes normally.

Cleanup:

```sh
kill -TERM "$lad_pid"
wait "$lad_pid" || true
rm -rf "$tmp"
```

## Long-run memory soak

Run a four-hour soak on Linux with a synthetic backend and monitor RSS:

```sh
tmp="$(mktemp -d)"
mkdir -p "$tmp/runtime" "$tmp/state" "$tmp/project"
target/debug/lad start \
  --socket "$tmp/runtime/lad-1.sock" \
  --state-dir "$tmp/state" \
  --test-shell-adapter "while :; do printf 'soak\n'; sleep 1; done" &
lad_pid=$!

for i in $(seq 1 240); do
  ps -o pid=,rss=,comm= -p "$lad_pid"
  sleep 60
done

kill -TERM "$lad_pid"
wait "$lad_pid" || true
rm -rf "$tmp"
```

Pass criteria: RSS remains bounded after warm-up; no monotonic growth trend
over the four-hour window, and the daemon exits cleanly on `SIGTERM`.
