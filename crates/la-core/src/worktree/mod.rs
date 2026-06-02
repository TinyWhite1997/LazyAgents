//! Per-session git worktree management for the LazyAgents daemon.
//!
//! Implements **WEK-27 / M2.4** plus the related sections of the M2
//! architecture brief on WEK-8 (§2 Worktree 隔离子系统, ADR-005).
//!
//! ## Responsibilities
//!
//! 1. Build a fresh, disposable `git worktree` for every
//!    `sessions.create` whose `worktree: true` flag is set.
//! 2. Run a single user-supplied advisory script
//!    (`.lazyagents/hooks/post-create.sh`) inside it, with a 60 s wall
//!    cap.
//! 3. Tear that worktree down when the session is archived or its TTL
//!    expires, preserving any branch that still carries un-merged
//!    commits.
//! 4. Surface every failure mode as a typed [`crate::CoreError`] so
//!    the dispatcher can pick a stable `-33110..-33119` RPC code.
//!
//! ## Layout and naming
//!
//! Worktrees live under the daemon's state dir:
//!
//! ```text
//! $XDG_DATA_HOME/lazyagents/worktrees/
//!     └── <project_slug>/
//!         └── <short_sid>/        ← actual `git worktree`
//! ```
//!
//! - `project_slug = sanitize(basename(repo_root)) + "-" + sha[..8]`.
//!   The 8-char hash disambiguates same-name roots (multiple
//!   `~/code/api/` checkouts) without depending on cwd ordering.
//! - `short_sid = first 12 + last 8 hex chars of the session UUID v7`.
//!   The prefix keeps `ls` roughly chronological; the tail keeps concurrent
//!   sessions created in the same timestamp bucket distinct.
//! - The branch name is `la/session-<short_sid>` — the `la/` namespace
//!   makes `git branch --list 'la/session-*'` a one-liner for ops.
//!
//! ## Hook contract
//!
//! After `git worktree add` succeeds, the manager looks for
//! `<repo_root>/.lazyagents/hooks/post-create.sh`. If present and
//! executable (unix), it runs with:
//!
//! - `cwd` = the new worktree path,
//! - `stdin` = `/dev/null`,
//! - `LA_*` env vars describing the session,
//! - a 60 s wall timeout.
//!
//! **Hook failure is advisory** — per WEK-8 brief amendment R4 it
//! does NOT roll back the worktree, abort the spawn, or mutate
//! `SessionStatus`. The outcome is recorded in
//! `sessions.post_create_hook_status` (`ok | failed | skipped |
//! timeout`) so the TUI can render a separate badge.
//!
//! ## Out-of-scope (deliberately)
//!
//! - No `git push`. The daemon never touches a remote.
//! - No `git rebase` / `git merge`. Brand-new worktrees are pinned to
//!   the base SHA at create time.
//! - No fs watcher. The diff panel (WEK-28) polls `worktree.status`
//!   on demand; a real watcher is M3 work.
//! - No config-driven hook path override. That's WEK-31 / M3.

pub mod diff;
mod git;
pub mod manager;
pub mod parser;

pub use diff::{
    CommitOutcome, DiffEngine, DiffLine, DiffOutcome, FileEntry, FileKind, FileStatus, Hunk,
    HunkReject, LaunchOutcome, MutationOutcome, StatusSnapshot, TruncationOutcome, WorktreeLocks,
    MAX_INLINE_DIFF_BYTES,
};
pub use git::classify_add_error;
pub use manager::{
    branch_name_for, project_slug, short_sid, CleanupMode, HookStatus, WorktreeHandle,
    WorktreeManager, WorktreePlan, HOOK_RELATIVE_PATH, POST_CREATE_HOOK_TIMEOUT,
};
pub use parser::{compute_hunk_id, LineOrigin, ParsedDiff, ParsedFile, ParsedHunk, ParsedLine};
