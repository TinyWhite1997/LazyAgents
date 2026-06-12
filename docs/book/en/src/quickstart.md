# Quickstart

**Goal:** zero to your first running session in under 5 minutes.

> **v1 status.** The daemon (`lad`) is fully functional in v1 — sessions, crons, worktrees, and adapter integrations all work. The TUI defaults to a live daemon-backed `RpcSessionSource` (with `la --demo` left in for the in-process fixture used in screenshots and design iteration), and the New-session form is wired end-to-end — pressing `n` opens a real backend / worktree picker that calls `sessions.create` on Confirm. The JSON-RPC path documented below stays valid for scripted spawns and CI.

## Before you start

You should already have:

1. `la` and `lad` installed (`la --version` works) — see [Install](install.md).
2. **At least one backend CLI installed and logged in.** LazyAgents does not handle authentication itself; it drives the CLI you already log in with.

| Backend | Install | Login |
|---|---|---|
| Claude Code | <https://docs.claude.com/en/docs/claude-code> | `claude login` |
| OpenAI Codex | <https://developers.openai.com/codex/cli> | `codex login` |
| sst.dev OpenCode | <https://opencode.ai/docs/> | `opencode auth login` |

Verify your backend works from the terminal first:

```sh
claude --version    # or codex --version, or opencode --version
```

If `--version` works but the tool says you're not logged in, fix that first. LazyAgents will surface the same error in step 4.

## 1. Launch the TUI

```sh
la
```

That's it. `la` checks whether `lad` is already running and runs `lad daemonize` in a `setsid`-detached child if not, so the daemon survives your terminal closing. The status bar at the bottom tells you which path you got:

| Status bar text | Meaning |
|---|---|
| `daemon @ <socket-path>` | A daemon was already up. You're connected. |
| `spawned lad @ <socket-path>` | `la` found `lad` and just started it. |
| `no daemon (lad not on PATH); start with 'lad daemonize'` | `la` can't find `lad`. Add it to `$PATH` or start the daemon yourself. |
| `no daemon (LAZYAGENTS_NO_AUTODAEMON set); expected at <path>` | You've disabled auto-spawn. Start `lad daemonize` yourself. |
| `daemon spawn failed: ...` | `lad` was found but couldn't start. The error text follows the colon. See [Troubleshooting → Daemon won't start](troubleshooting.md#daemon-wont-start). |

## 2. The v1 UX caveat

LazyAgents v1 ships the full daemon — sessions, crons, worktrees, adapters all work. **The TUI is wired end-to-end against the live daemon**:

- Pressing `n` on a project opens the New-session form: pick a backend, toggle the worktree flag, and `Enter` to create. The session starts with no initial prompt — attach and type your first instruction into the live agent. The new session appears on the sidebar within the next ~2 s refresh tick.
- Pressing `Enter` on a session row opens a full terminal-emulator pane that renders the agent's TUI faithfully and routes your keystrokes back to the daemon via `sessions.write`. The pane is sized to your window (`sessions.resize`). Press `Ctrl+\` to detach — the session keeps running on the daemon. Every other key is forwarded verbatim to the agent.

If you want to drive `la` without a daemon (regression screenshots, design iteration), pass **`la --demo`** — that reverts the sidebar to the in-process demo fixture. The default `la` invocation always talks to `lad` and refuses to inject demo data into a real workspace.

You can still drive the daemon directly via JSON-RPC (and many CI flows will) — the rest of this chapter shows that path so the same example works whether you came in through the TUI or a scripted client.

## 3. Drive a real session through the daemon

You're going to talk JSON-RPC over the Unix socket. Any tool that can send length-prefixed framed JSON works; here's a one-shot in Python:

```python
import json, os, socket, struct

sock_path = os.path.expandvars("$XDG_RUNTIME_DIR/lazyagents/lad-1.sock")
s = socket.socket(socket.AF_UNIX); s.connect(sock_path)

def send(msg):
    body = json.dumps(msg).encode()
    s.sendall(struct.pack(">I", len(body)) + body)

def recv():
    (n,) = struct.unpack(">I", s.recv(4))
    return json.loads(s.recv(n))

send({"jsonrpc":"2.0","id":1,"method":"initialize",
      "params":{"client":"quickstart","client_version":"0.0.1",
                "protocol_versions":["1"]}})
print(recv())

send({"jsonrpc":"2.0","id":2,"method":"sessions.create",
      "params":{"project_dir": os.path.expanduser("~/code/myapp"),
                "backend": "claude",
                "worktree": False,
                "prompt":  "Add a README about the build system."}})
print(recv())
```

The response carries the `session_id`. Subscribe to `events.subscribe` with topic `session.output` and `sessions.attach` with `resume_from_seq: 0` to get the live PTY bytes; full method list is in the [Sessions chapter](sessions.md#rpc-reference).

## 4. The "I closed my terminal" test

This is the whole point. With a session spawned via path B above:

```sh
# Close your terminal, log out, even reboot.
# Come back. Reconnect to the socket.
# sessions.list { include_archived: false } shows your session, still running.
# sessions.attach { session_id, resume_from_seq: <last> } resumes the stream
# from where you left off, replaying the in-memory ring buffer (2 MiB per session).
```

Output missed during your absence is replayed from the ring on reattach. If the ring overflowed (you were gone for a long, chatty session), a `session.gap` notification tells you the dropped seq range — the data is still in the persisted transcript.

## 5. Watch it survive a reboot

The daemon does *not* automatically respawn on reboot (no systemd unit is installed). After a reboot:

```sh
la
```

`la` will auto-spawn `lad`, which loads everything back from SQLite. Active sessions whose PTY children were killed by the reboot get marked `exited`; transcripts are preserved.

## What just happened, behind the scenes

- `la` connected to `lad` over a `0600`-permissioned Unix socket at `$XDG_RUNTIME_DIR/lazyagents/lad-1.sock` (UID-checked via `SO_PEERCRED`).
- `lad` spawned your backend CLI through a portable PTY (`portable-pty`), tagged each output chunk with a monotonic sequence number, and wrote them to a SQLite-backed transcript.
- When you detached, the PTY stayed alive — the daemon owns it, not your terminal.

## Next steps

- Schedule that same backend: [Crons →](crons.md)
- Have the agent work on its own branch and review its diff: [Worktree review →](worktree.md)
- Pull in sessions you already started with `claude`/`codex`/`opencode` outside LazyAgents: [Sessions → Discover & import](sessions.md#discover-and-import-existing-sessions)
- Configure your theme and keyboard hints: [Sessions → UI preferences](sessions.md#ui-preferences-themecompactkey-hints)
