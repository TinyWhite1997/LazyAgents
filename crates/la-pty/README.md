# la-pty

Cross-platform PTY abstraction for LazyAgents. Wraps
[`portable-pty`](https://crates.io/crates/portable-pty) and exposes a
tokio-friendly handle:

```rust
use la_pty::{spawn, CommandBuilder, PtySize, Signal};

let mut cmd = CommandBuilder::new("cat");
let mut child = spawn(cmd, PtySize::default())?;

child.writer.write(&b"hello\n"[..]).await?;
if let Some(chunk) = child.reader.recv().await {
    // chunk: bytes::Bytes
}
child.resize(PtySize { rows: 50, cols: 132, pixel_width: 0, pixel_height: 0 })?;
child.signal(Signal::Interrupt)?;
let status = child.wait().await?;
```

This crate is `la-pty` from the LazyAgents workspace (see
`report/技术架构设计.md` §2, §6 ADR-004). It is intentionally a thin layer:
no IPC, no SQLite, no adapter logic. The contract: `PtyChild { reader,
writer, resize, signal, wait }`, with the §6.2 read loop wired in
(`spawn_blocking`-style dedicated thread + bounded `mpsc` so OS-side
back-pressure flows from a slow consumer all the way back to the child).

## API summary

| Item | Purpose |
|------|---------|
| `spawn(cmd, size) -> PtyChild` | Open a PTY pair, spawn `cmd` on the slave, wire up read/write threads. |
| `PtyChild.reader: mpsc::Receiver<Bytes>` | PTY output. Closes on EOF. |
| `PtyChild.writer: PtyWriter` | Async input. Bounded channel (1024) — `write().await` exerts back-pressure when the writer thread is slow. |
| `PtyChild.resize(PtySize)` | Window resize. Thread-safe. |
| `PtyChild.signal(Signal)` | Cross-platform interrupt/terminate/kill. |
| `PtyChild.wait().await` | Reap the child, return `ExitStatus`. |

## Cross-platform behavior differences

| Topic | Unix (Linux / macOS) | Windows (ConPTY) |
|-------|----------------------|------------------|
| **PTY backend** | `openpty(3)` via `portable-pty`'s `UnixPtySystem`; slave runs in a new session (`setsid`), so the child has its own controlling tty. | ConPTY (`CreatePseudoConsole`) via `portable-pty`'s `ConPtySystem`. Requires Windows 10 1809+. |
| **Process group / isolation** | `portable-pty` calls `setsid` on the slave fork. Process group ID == child PID. We send signals to the *process group* (`killpg`), matching real terminal Ctrl-C semantics. | `portable-pty` spawns with `CREATE_NEW_PROCESS_GROUP`. Without that flag, `GenerateConsoleCtrlEvent` would broadcast to the daemon itself — do not remove. |
| **`Signal::Interrupt` (Ctrl-C)** | `SIGINT` to pgrp. | `GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid)`. Child must be in its own group (see above). |
| **`Signal::Terminate`** | `SIGTERM` to pgrp. | `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)`. Note: CTRL_C is *catchable*, CTRL_BREAK is *also* catchable but more reliably triggers default termination in console apps. |
| **`Signal::Kill`** | `SIGKILL` to pgrp (cannot be caught). | `OpenProcess(PROCESS_TERMINATE) → TerminateProcess(handle, 1)`. Process-only — does not kill descendants; pair with job objects if you need a tree-kill (not provided here). |
| **EOF detection** | Reader thread returns `Ok(0)` when the slave fd closes (child exit). We `drop(pair.slave)` after spawn so the master sees EOF as soon as the child's stdio is gone. | ConPTY signals EOF when the pseudoconsole is closed and the attached process exits. Same `Ok(0)` path. |
| **ANSI output** | Whatever the child writes, byte-for-byte. | ConPTY injects extra cursor / OSC sequences (cursor position queries, mode-change reports). Renderers should tolerate via a VTE parser (see §6.5 of the architecture doc). |
| **Resize** | `TIOCSWINSZ` ioctl via `portable-pty`. Sends `SIGWINCH` to the foreground process. | `ResizePseudoConsole`. Does not raise `SIGWINCH` (there is no such signal); console apps poll `GetConsoleScreenBufferInfo`. |
| **`PtyChild.pid()`** | The PID of the spawned child. Stable for the life of the child. | Same. |
| **Environment** | Child inherits whatever was put in `CommandBuilder.env(...)`. We do *not* sanitize the daemon's env automatically — callers should pass an explicit whitelist (see architecture §6.5 / §10.3). | Same. |
| **`cwd`** | `CommandBuilder.cwd(...)`. Must be a valid absolute path. | Same. Long-path (`\\?\...`) handling is portable-pty's responsibility. |

## Backpressure model (architecture §6.2)

The read loop runs on a dedicated OS thread (not `tokio::task::spawn_blocking`
— this keeps the tokio blocking pool free under bursty PTY output):

```text
PTY master ── reader.read() ──► [bounded mpsc, cap 1024] ──► consumer
                                          ▲
                                          │ blocking_send blocks here
                                          │ when consumer is slow,
                                          │ which in turn stops draining
                                          │ the OS PTY buffer, which
                                          │ eventually blocks the child's
                                          │ write() syscall.
```

The write side is symmetric: callers `await PtyWriter::write(bytes)`,
which pushes onto a bounded mpsc drained by a second dedicated thread.

## Testing

```bash
cargo test -p la-pty
```

The smoke suite (`tests/smoke.rs`) covers:

- `spawn_echo_reads_output_and_sees_eof` — spawn → read → EOF → wait.
- `cat_round_trip_write_then_read` — bidirectional I/O.
- `resize_does_not_error` — resize calls succeed mid-flight.
- `signal_interrupt_terminates_child` — Ctrl-C maps correctly.
- `signal_kill_terminates_child` — hard kill maps correctly.

All five must pass on `ubuntu-latest`, `macos-latest`, and
`windows-latest` (architecture doc §12).

## Non-goals

- No tree-kill / job-object lifecycles. The daemon's session manager
  composes these on top.
- No transcript persistence — that's `la-storage`.
- No adapter-specific parsing — that's `la-adapter`.
- No IPC. The daemon owns the bridge between `PtyChild` events and the
  RPC bus.

## Dependencies

- `portable-pty` — OS PTY abstraction (already battle-tested in wezterm).
- `tokio` (sync/time/rt) — async glue.
- `bytes` — zero-copy buffer transfer.
- `nix` (Unix only) — signals.
- `windows-sys` (Windows only) — `GenerateConsoleCtrlEvent`, `TerminateProcess`.

No container, scripting, or shell dependency, per the acceptance
criteria of WEK-11.
