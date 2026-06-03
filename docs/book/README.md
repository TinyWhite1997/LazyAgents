# LazyAgents user documentation

The user-facing handbook lives here as two parallel mdBook sites:

```
docs/book/
├── en/      ← English book
├── zh-CN/   ← Simplified Chinese book
└── README.md
```

## Build

### Install mdBook

```sh
cargo install mdbook --locked
```

(Any 0.4.x release works. CI uses whatever the `dtolnay/install` step installs by default.)

### Build both books

From the repo root:

```sh
mdbook build docs/book/en
mdbook build docs/book/zh-CN
```

Output lands at `docs/book/build/en/` and `docs/book/build/zh-CN/` (one HTML site each). Open `index.html` in a browser or serve it:

```sh
mdbook serve --open docs/book/en       # http://localhost:3000
```

`mdbook serve` watches for changes and auto-rebuilds.

### Edit and preview

Source lives in `docs/book/<lang>/src/`. The chapter list is `SUMMARY.md`. Edit any `.md` file, save, and `mdbook serve` will hot-reload.

## Style

- Second person ("you do X"), present tense, active voice.
- Cite exact paths and env vars (e.g. `$XDG_DATA_HOME/lazyagents/lad.sqlite`, `LAZYAGENTS_LOG`) so users can copy-paste.
- Quote actual error strings (`unauthenticated; see <docs_url>`) so users can grep their terminal for them.
- Keep chapters self-contained: a reader who lands on `worktree.md` from a search should not need to read `install.md` first.

## Chapter responsibilities

| File | Role |
|---|---|
| `README.md` | Landing page — what LazyAgents is, who it's for, where to start. |
| `install.md` | Get the binaries on the user's machine; verify install. |
| `quickstart.md` | Zero-to-first-session in under 5 minutes. The onboarding success metric. |
| `sessions.md` | Day-to-day session usage: lifecycle, attach, replay, import, UI prefs. |
| `crons.md` | The scheduler: grammar, DST, enable gate, catch-up, failure handling. |
| `worktree.md` | Per-session git worktree review. |
| `adapters.md` | Backends: claude, codex, opencode. Auth, discovery, resume. |
| `troubleshooting.md` | Symptom → cause → fix table. Platform-specific known issues. |
| `faq.md` | Common questions; complement to Troubleshooting (questions, not symptoms). |

## When you ship a feature

If your change adds, removes, or renames a user-visible RPC method, TUI keystroke, config key, env var, or file path — update the docs **in the same PR**. Specifically:

- New session lifecycle state → `sessions.md` lifecycle table.
- New cron field → `crons.md` editor table + persistence table.
- New worktree subcommand or notification topic → `worktree.md`.
- New backend adapter → `adapters.md` table + per-adapter section.
- New CLI flag on `la` or `lad` → `install.md` and the relevant chapter.

If you can't update both languages, raise an issue for the translation gap so it gets picked up before the next release.

## Build output

`docs/book/build/` is git-ignored. Production hosting (gh-pages or Vercel — TBD) should run both `mdbook build`s and serve the two trees under `/en/` and `/zh-CN/`.
