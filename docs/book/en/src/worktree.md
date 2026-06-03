# Worktree review

When you create a session with `worktree: true`, LazyAgents runs `git worktree add -b la/session-<short-sid> <base>` before spawning the backend. The agent now works on its own branch in its own working directory, completely isolated from your main checkout — and you get a built-in diff review for everything it changed.

## What the daemon sets up

On `sessions.create { worktree: true }`:

1. Resolve the base branch: `origin/HEAD` first, falling back to local `HEAD`.
2. Run `git worktree add -b la/session-<short-sid> <base-sha>` to atomically create the branch and check it out.
3. Write the session row to SQLite with `worktree_path` and `worktree_branch` populated.
4. If `<project>/.lazyagents/hooks/post-create.sh` exists and is executable, run it (60 s budget, advisory).

If any step before the session row is persisted fails, the daemon runs `git worktree remove --force` and `git branch -D` to roll back. You won't end up with orphan branches from a failed spawn.

## Where worktrees live

```
<state_dir>/worktrees/<project-slug>/<short-sid>/
```

- `state_dir`: `$LAZYAGENTS_DATA_DIR` or `$XDG_DATA_HOME/lazyagents` or `~/.local/share/lazyagents`.
- `project-slug`: `<basename-of-repo>-<8-char-sha256-of-abs-path>`. The hash disambiguates two checkouts that happen to share a directory name.
- `short-sid`: first 16 hex chars of the session UUID v7. The v7 timestamp prefix keeps `ls` output chronological.

Example: a session for `/home/alice/code/myapp` lands at `/home/alice/.local/share/lazyagents/worktrees/myapp-a1b2c3d4/018e12345678abcd/`, on branch `la/session-018e12345678abcd`.

The `la/` branch namespace is your operator escape hatch: `git branch --list 'la/session-*'` lists every LazyAgents-provisioned branch in a repo.

## Requirements

- `git` ≥ 2.20 on `$PATH` — the daemon shells out to it for every worktree operation. There is no libgit2 fallback.
- The daemon enforces `LC_ALL=C` on every git subprocess to pin error messages to English (which is what its error classifier matches against).
- Every git subprocess has a hard 30-second wall-clock timeout. A timeout returns a `WorktreeProvision` error.

If git is missing, you'll see `the git binary was not found on $PATH` — install git and re-run.

## Review the diff

From the TUI session view (specifically the worktree pane added in M2.5), you have seven commands:

| Command | Effect |
|---|---|
| `worktree.status` | Snapshot: branch, base, head, ahead/behind, per-file status. |
| `worktree.diff` | Hunks for one file, staged or unstaged. |
| `worktree.stage` | Move listed hunks from working tree → index. |
| `worktree.unstage` | Move listed hunks from index → working tree. |
| `worktree.discard` | Throw listed hunks away (requires explicit confirmation — see below). |
| `worktree.commit` | `git commit -F -` with your message. |
| `worktree.open_in_editor` | Launch your editor at a path:line:col. Fire-and-forget. |

Every hunk in the diff carries a stable `hunk_id` (a 16-char SHA-256 fingerprint over path + range + body bytes). Stage, unstage, and discard all operate by hunk_id. If the file moved while you were reading the diff, stale ids are reported in the response's `rejected` array instead of silently mis-applying — re-fetch the diff and retry.

### Diff sizing

- Files larger than **5 MiB** are not inlined. The diff response carries `truncated.hint: "open_in_editor"` and `hunks: []`. Open it in your editor instead.
- Binary files, submodules, and unsupported file kinds are also truncated.
- The `context_lines` field is accepted for forward-compatibility but **currently ignored** — the daemon always uses `-U3`.

### Discarding is gated

`worktree.discard` refuses to do anything unless you send `confirmed: true`. Older clients that don't know about that field default to `false` and get a `WORKTREE_DISCARD_UNCONFIRMED` error — a safe fail-closed default that prevents accidental data loss.

The TUI shows a confirmation modal; the safe answer is "no".

### Commit

`git commit -F -` with your message on stdin. Always honours your repo's `pre-commit` and `commit-msg` hooks — `--no-verify` is never set. The daemon will tell you if a hook rejected the commit.

Explicitly out of scope (today): `--amend`, `--signoff`, GPG flag toggling, and auto-push. Commit `--amend` against an in-progress agent session is a footgun we deliberately don't enable.

### Open in editor

`worktree.open_in_editor` resolves your editor in this order:

1. The `editor_override` param if set.
2. `$VISUAL` if set and non-empty.
3. `$EDITOR` if set and non-empty.
4. `code` (VS Code) if found on `$PATH`.
5. Otherwise: `WorktreeEditorUnavailable` error.

`$EDITOR` strings with embedded flags work: `EDITOR="code --wait"` is split on whitespace and the flags are prepended.

Per-editor argument syntax (auto-detected from the binary name):

| Editor binary | Line argument | Column support |
|---|---|---|
| `code`, `code-insiders`, `cursor`, `vscodium`, `windsurf` | `--goto path:line:col` | yes |
| `zed`, `zeditor` | `path:line:col` | yes |
| `idea`, `pycharm`, `webstorm`, `rustrover`, `clion` | `--line <n> path` | no |
| `vim`, `nvim`, `vi` | `+<n> path` | no |
| `emacs`, `emacsclient` | `+<n> path` | no |
| anything else | `path` only | no |

The editor is spawned fire-and-forget. The daemon returns as soon as the child PID is known; it never waits for the editor to exit.

## Notifications

Subscribe to `worktree.changed` (any mutation) and `worktree.commit_created` (commit-specific, includes summary + files_changed count) via `events.subscribe` to react to changes. The TUI uses these to refresh the diff pane and show toast messages without polling.

A `worktree.changed` with `kind: "external"` is reserved for changes made outside the daemon (e.g. an agent or editor writing directly to the worktree). It is not currently emitted by the daemon — your tooling has to detect external changes itself.

## Archive vs delete (worktree side)

When you archive a session, the daemon:

- Removes the worktree directory (`git worktree remove --force`).
- **Preserves the branch** if it has commits beyond `base_branch` (`CleanupMode::KeepBranchIfDirty`). The session row's `worktree_path` clears; `worktree_branch` stays so the TUI can offer a `git checkout` later.

A background sweep tears down archived worktrees and branches after the retention TTL (7 days by default), this time with `CleanupMode::Force`.

Hard `sessions.delete` does **not** touch the worktree directory or branch — by design, since "delete" already implies cascading-deletes in SQLite and we don't want it to silently destroy un-merged commits. If you want both gone, archive first and let the sweeper clean up.

## When git complains

`git worktree add` failures are surfaced verbatim through `CoreError::WorktreeProvision`. Common ones:

| Error fragment | Meaning | Fix |
|---|---|---|
| `'la/session-...' is already used by worktree at ...` | A previous attempt left the branch around. | `git worktree prune && git branch -D la/session-<sid>` and try again. |
| `fatal: not a git repository` | The project root isn't a git repo. | Initialise it (`git init`) or pick a different project. |
| `fatal: invalid reference: origin/HEAD` | No `origin/HEAD` symref. | LazyAgents falls back to local `HEAD`; this should be informational, not fatal. File an issue if it stops you. |

Manual recovery is always safe: `git worktree list` shows everything (including LazyAgents-provisioned ones), and `git worktree remove --force <path>` undoes whatever the daemon did.
