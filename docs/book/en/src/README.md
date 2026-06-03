# LazyAgents

> Keep your coding agents alive across reboots, restarts, and SSH disconnects — without nailing your laptop to a tmux session.

LazyAgents is a long-running local **daemon** (`lad`) and a fast **TUI** (`la`) that babysits unattended coding agents — Claude Code, OpenAI Codex, sst.dev OpenCode — for you. Sessions survive your terminal closing, cron jobs run them while you sleep, and a built-in worktree review lets you stage and commit what the agent produced.

## What you get

- **Persistent sessions.** Detach, log out, reboot. Reattach and pick up where the agent left off.
- **Scheduled runs.** A real cron with IANA timezones and DST-aware semantics — perfect for nightly refactors and morning code reviews.
- **Git worktree per session.** Each agent gets its own branch and working copy. Stage, unstage, discard, and commit hunks from inside the TUI.
- **Three backends out of the box.** `claude`, `codex`, `opencode`. Bring your own login.
- **Local-only.** No network listener, no telemetry, no upload. Your sessions and your data stay on your machine.

## Who this is for

You are someone who runs `claude`, `codex`, or `opencode` long enough that babysitting a single terminal window becomes a chore. You want the agent to keep working when you walk away — and you want a single place to see all the work it has done across projects.

## Scope of this release (v1)

LazyAgents is actively developed. **v1 is validated on Linux only.** The code compiles for macOS and Windows, and our release pipeline produces binaries for all five targets, but cross-platform smoke testing is a follow-up before GA. If you are on macOS or Windows, expect rough edges — see [Troubleshooting](troubleshooting.md) for the known issues.

## Pick a path

- **First time here?** Start with [Install](install.md), then [Quickstart](quickstart.md). You should have a session running in under 5 minutes.
- **Already installed?** Jump to [Sessions](sessions.md) or [Crons](crons.md).
- **Something broken?** [Troubleshooting](troubleshooting.md) and the [FAQ](faq.md).

## Project links

- Source code: <https://github.com/TinyWhite1997/LazyAgents>
- Releases (binaries): <https://github.com/TinyWhite1997/LazyAgents/releases>
- License: MIT
