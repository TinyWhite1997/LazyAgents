//! `SessionSource`: abstraction over "where do sessions come from".
//!
//! The TUI does not own state; the daemon does. But we want the sidebar
//! testable without spinning up a real `lad` (and we want the binary to be
//! runnable for design iteration before M1.7 lands the daemon). So both code
//! paths go through this trait.
//!
//! The trait is intentionally synchronous and snapshot-based: the sidebar
//! periodically refreshes by calling [`SessionSource::snapshot`] (the
//! daemon-backed impl will be a thin async cache behind the scenes). A
//! mutation method ([`SessionSource::archive`] / [`SessionSource::delete`])
//! returns immediately and the next snapshot reflects the change — this
//! mirrors the daemon's actual semantics where `sessions.archive` is a
//! fire-and-event flow.

use std::collections::BTreeMap;
use std::fmt;

use crate::model::{Backend, ProjectGroup, RunState, SessionRow};

pub mod rpc;
pub use rpc::RpcSessionSource;

/// Newly-assigned session identifier returned by
/// [`SessionSource::create_session`].
///
/// Kept as a thin newtype over [`String`] so the trait surface does not
/// drag in a `uuid` dependency (mock IDs are `mock-N` strings) but
/// callers can tell at a glance whether they are holding a freshly
/// minted id vs an arbitrary session-id reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn into_string(self) -> String {
        self.0
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Request payload for [`SessionSource::create_session`]. Field names and
/// types mirror [`la_proto::methods::SessionsCreateParams`] so the
/// `RpcSessionSource` translation is a no-op.
///
/// `args` is plumbed through as a future-proofing slot — A2 does not yet
/// expose extra args from the modal, but reserving the field now avoids
/// a trait churn when a later milestone wires per-backend launch flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionRequest {
    /// Absolute path of the project directory the agent should `cd` into.
    pub project_dir: String,
    /// Backend identifier — must match one of the names in
    /// [`la_proto::methods::ServerCapabilities::adapters`].
    pub backend: String,
    /// Extra args appended to the adapter's base command. Empty until a
    /// later milestone surfaces an args buffer in the modal.
    pub args: Vec<String>,
    /// If true, create a fresh git worktree for this session.
    pub worktree: bool,
}

/// Newly-registered project identifier returned by
/// [`SessionSource::create_project`].
///
/// In M1 the daemon has no `projects.create` RPC — [`ensure_project`] on
/// the daemon side lazily creates the SQLite row when the first
/// `sessions.create` lands. The TUI therefore manages "the user
/// announced this directory as a project" entirely in-process: the
/// source registers it locally, the sidebar surfaces it as an empty
/// group, and the daemon picks the same `root_path` up the moment the
/// user spawns their first session inside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectId(pub String);

impl ProjectId {
    pub fn into_string(self) -> String {
        self.0
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reason [`SessionSource::create_session`] refused.
///
/// Split into two arms so the calling [`crate::app::App`] can decide
/// whether to keep the modal open (for a validation slip the user can
/// correct in place) or close it and surface a toast (for a backend /
/// daemon failure the user cannot fix from the modal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// Caller-side validation failed — missing backend, empty project
    /// directory, etc. Modal stays open so the user can correct the input.
    Validation(String),
    /// Daemon / IPC layer refused or the round-trip failed. Modal
    /// closes; the message is surfaced via the status toast.
    Backend(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Validation(s) => write!(f, "{s}"),
            SourceError::Backend(s) => write!(f, "{s}"),
        }
    }
}

impl SourceError {
    /// True for [`SourceError::Validation`] — callers use this to keep
    /// the modal open after a refused Confirm.
    pub fn is_validation(&self) -> bool {
        matches!(self, SourceError::Validation(_))
    }
}

/// Read/mutate the session view shown in the sidebar.
///
/// Implementations are expected to be cheap to call (in-memory cache);
/// the IPC-backed impl in the `la` binary refreshes via `sessions.list`
/// notifications instead of polling on every key event.
pub trait SessionSource {
    /// Current set of project groups, ordered for display.
    ///
    /// Implementations MUST place the synthetic Archived group last when
    /// any archived session exists; the sidebar relies on that ordering for
    /// PRD §5.3 ("末尾固定 Archived 分组"). The synthetic Discovered
    /// group, when present, sits just above the Archived bucket so the
    /// project-list order isn't disturbed.
    fn snapshot(&self) -> Vec<ProjectGroup>;

    /// Move a session into the Archived bucket. No-op if `session_id` is
    /// unknown — the source is the authority on existence.
    fn archive(&mut self, session_id: &str);

    /// Permanently remove a session. Caller is expected to have shown a
    /// confirmation modal first (PRD §5.3: 二次确认).
    fn delete(&mut self, session_id: &str);

    /// Restore a previously archived session back to its project group.
    fn restore(&mut self, session_id: &str);

    /// Promote a discovered (read-only) session into a native session
    /// row. `session_id` here is the discovered row's `external_id` —
    /// the daemon-backed implementation passes it through
    /// `sessions.import`; the mock implementation just flips the row's
    /// `discovered` flag so the snapshot moves it back under its
    /// originating project. No-op for unknown / already-imported ids.
    fn import_discovered(&mut self, _session_id: &str) {}

    /// Spawn a new session against the given backend + project dir.
    ///
    /// Returns the daemon-assigned [`SessionId`] on success. The mock
    /// impl synthesises a deterministic id (`mock-N`) and inserts a
    /// `RunState::Running` row under the requested project so the
    /// sidebar reflects the new session on the next snapshot. The
    /// [`RpcSessionSource`] translates this into a `sessions.create`
    /// RPC + a follow-up `sessions.list` refresh.
    fn create_session(&mut self, req: NewSessionRequest) -> Result<SessionId, SourceError>;

    /// Register an existing on-disk directory as a project.
    ///
    /// Returns the project id the sidebar should focus after the
    /// refresh. M1's daemon has no `projects.create` RPC — the storage
    /// row is created lazily on the first `sessions.create` against
    /// this directory — so the source MUST keep a local record of the
    /// directory so the sidebar can show the (empty) project group
    /// immediately. The next `create_session` against the same path
    /// will reuse the daemon's `ensure_project` path and the local
    /// record naturally folds into the daemon-side snapshot.
    ///
    /// Implementations MUST:
    /// - reject empty / non-absolute paths and paths that don't exist
    ///   on disk with [`SourceError::Validation`] (the issue brief
    ///   forbids creating directories from the modal),
    /// - reject a path that's already registered with a Validation
    ///   error so the App can surface a "project already exists" toast,
    /// - mint a stable id so the App can pre-position the sidebar cursor
    ///   onto the new group before the next snapshot lands.
    ///
    /// Default implementation refuses with a Backend error so future
    /// sources don't silently no-op — every real source must opt in.
    fn create_project(&mut self, _path: &str) -> Result<ProjectId, SourceError> {
        Err(SourceError::Backend(
            "this session source does not support creating projects".into(),
        ))
    }

    /// Monotonic counter the source bumps after every internal cache
    /// rebuild. The runner reads this once per frame and dispatches
    /// [`crate::app::AppMsg::RefreshSessions`] whenever the value
    /// changes, so a bg-thread refresh (poll tick, mutation re-pull,
    /// future push notification) reaches the sidebar within one frame
    /// (~250 ms) instead of waiting for a user keystroke. Sources
    /// without a background thread (the in-memory mock) can leave the
    /// default `0`; the runner treats "never changes" as "never has
    /// new data".
    fn refresh_generation(&self) -> u64 {
        0
    }
}

/// In-memory `SessionSource` used by tests and the binary's `--demo` mode.
///
/// Stores raw rows by id plus a `project -> (name, root_path)` map. Rebuilds
/// the grouped snapshot on every call, since the row count is small (tens,
/// not thousands) and the simpler invariant beats incremental upkeep.
pub struct MockSessionSource {
    /// All sessions, archived or not, keyed by session id.
    rows: BTreeMap<String, SessionRow>,
    /// Project metadata; we look it up by project id during snapshot
    /// rendering. `display_order` carries the deterministic ordering across
    /// snapshots so the sidebar does not jitter on a refresh.
    projects: Vec<ProjectMeta>,
    /// Monotonic id source for `create_session`. Synthesised id is
    /// `mock-<n>` so tests can assert deterministic shapes without
    /// dragging in a uuid dependency on the mock.
    next_create_id: u64,
    /// Monotonic id source for `create_project` — mints `mock-proj-<n>`
    /// so the App's "register a new project" path stays testable
    /// without a uuid dep on the mock.
    next_project_id: u64,
    /// Tape of every `create_session` call, captured for unit tests
    /// that pin the UI ↔ trait wire-up.
    created: Vec<NewSessionRequest>,
}

struct ProjectMeta {
    project_id: String,
    display_name: String,
    root_path: String,
}

impl MockSessionSource {
    pub fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
            projects: Vec::new(),
            next_create_id: 0,
            next_project_id: 0,
            created: Vec::new(),
        }
    }

    /// Inspect the tape of [`SessionSource::create_session`] calls. Used
    /// by unit tests to pin that the modal hands the trait the exact
    /// fields the user selected.
    pub fn created(&self) -> &[NewSessionRequest] {
        &self.created
    }

    /// Register a project + its display metadata. Order of registration is
    /// preserved in the snapshot.
    pub fn add_project(
        &mut self,
        project_id: impl Into<String>,
        display_name: impl Into<String>,
        root_path: impl Into<String>,
    ) {
        self.projects.push(ProjectMeta {
            project_id: project_id.into(),
            display_name: display_name.into(),
            root_path: root_path.into(),
        });
    }

    /// Insert a session into a previously-registered project. `project_id`
    /// not matching any registered project is silently dropped under the
    /// archived bucket on snapshot, which lets tests exercise stale-data
    /// scenarios without panic.
    pub fn add_session(
        &mut self,
        session_id: impl Into<String>,
        project_id: impl Into<String>,
        backend: &str,
        title: Option<&str>,
        run_state: RunState,
    ) {
        let session_id = session_id.into();
        let row = SessionRow {
            session_id: session_id.clone(),
            project_id: project_id.into(),
            backend: Backend::new(backend),
            title: title.map(str::to_string),
            run_state,
            archived: false,
            discovered: false,
        };
        self.rows.insert(session_id, row);
    }

    /// Insert a discovered (read-only) session under the Discovered
    /// bucket. The mock keeps the row's `project_id` set to its
    /// originating project so `import_discovered` can flip the flag and
    /// have the snapshot route the row back under its real project.
    pub fn add_discovered_session(
        &mut self,
        session_id: impl Into<String>,
        project_id: impl Into<String>,
        backend: &str,
        title: Option<&str>,
    ) {
        let session_id = session_id.into();
        let row = SessionRow {
            session_id: session_id.clone(),
            project_id: project_id.into(),
            backend: Backend::new(backend),
            title: title.map(str::to_string),
            run_state: RunState::Exited,
            archived: false,
            discovered: true,
        };
        self.rows.insert(session_id, row);
    }

    /// Construct a small fixture useful for the demo binary and for hand
    /// inspection during design review (matches the ASCII mock in PRD §5.1).
    pub fn fixture() -> Self {
        let mut s = Self::new();
        s.add_project("p-a", "proj-a", "~/code/proj-a");
        s.add_project("p-b", "proj-b", "~/code/proj-b");
        s.add_session(
            "01934fff-feed-7000-a000-aaaaaaaaa001",
            "p-a",
            "claude",
            Some("Refactor auth"),
            RunState::Running,
        );
        s.add_session(
            "01934fff-feed-7000-a000-aaaaaaaaa002",
            "p-a",
            "codex",
            None,
            RunState::Idle,
        );
        s.add_session(
            "01934fff-feed-7000-a000-bbbbbbbbb001",
            "p-b",
            "opencode",
            Some("Long task"),
            RunState::Running,
        );
        s.add_session(
            "01934fff-feed-7000-a000-bbbbbbbbb002",
            "p-b",
            "claude",
            None,
            RunState::Waiting,
        );
        // Pre-archive one to exercise the Archived bucket.
        let archived_id = "01934fff-feed-7000-a000-ccccccccc001".to_string();
        s.add_session(
            archived_id.clone(),
            "p-b",
            "claude",
            Some("Old experiment"),
            RunState::Exited,
        );
        s.archive(&archived_id);
        // One discovered session per backend so the bucket is non-empty
        // in the demo binary (WEK-26).
        s.add_discovered_session(
            "discovered-codex-abc",
            "p-a",
            "codex",
            Some("Codex chat (discovered)"),
        );
        s.add_discovered_session(
            "discovered-claude-xyz",
            "p-b",
            "claude",
            Some("Claude chat (discovered)"),
        );
        s
    }
}

impl Default for MockSessionSource {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionSource for MockSessionSource {
    fn snapshot(&self) -> Vec<ProjectGroup> {
        let mut groups: Vec<ProjectGroup> = self
            .projects
            .iter()
            .map(|p| {
                let mut g = ProjectGroup::new(&p.project_id, &p.display_name);
                g.root_path = p.root_path.clone();
                g
            })
            .collect();
        let mut discovered = ProjectGroup::discovered();
        let mut archived = ProjectGroup::archived();
        for row in self.rows.values() {
            if row.discovered {
                discovered.sessions.push(row.clone());
                continue;
            }
            if row.archived {
                archived.sessions.push(row.clone());
                continue;
            }
            if let Some(g) = groups.iter_mut().find(|g| g.project_id == row.project_id) {
                g.sessions.push(row.clone());
            }
            // Sessions whose project_id is unknown are dropped silently
            // (test fixtures that point at non-registered projects).
        }
        if !discovered.sessions.is_empty() {
            groups.push(discovered);
        }
        if !archived.sessions.is_empty() {
            groups.push(archived);
        }
        groups
    }

    fn archive(&mut self, session_id: &str) {
        if let Some(row) = self.rows.get_mut(session_id) {
            row.archived = true;
        }
    }

    fn delete(&mut self, session_id: &str) {
        self.rows.remove(session_id);
    }

    fn restore(&mut self, session_id: &str) {
        if let Some(row) = self.rows.get_mut(session_id) {
            row.archived = false;
        }
    }

    fn import_discovered(&mut self, session_id: &str) {
        if let Some(row) = self.rows.get_mut(session_id) {
            row.discovered = false;
        }
    }

    fn create_project(&mut self, path: &str) -> Result<ProjectId, SourceError> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err(SourceError::Validation("path is required".into()));
        }
        // The Mock does NOT touch the real filesystem (tests run in
        // tmpfs-less environments / CI sandboxes) — checking existence
        // here would make every test that drives the modal pay a tmp
        // dir + mkdir. Existence enforcement lives on the modal side
        // for the live source; the mock's job is to exercise the
        // dedup + select-after-create paths.
        //
        // Dedup is by `root_path` ONLY — never by `display_name`. The
        // daemon's `projects` table only enforces uniqueness on
        // `root_path`, and two distinct checkouts can share a basename
        // (`/repo/app` and `/tmp/app`); collapsing on display_name was
        // the Code Reviewer's blocker on PR #92.
        let norm = trimmed.trim_end_matches(['/', '\\']);
        if self
            .projects
            .iter()
            .any(|p| p.root_path.trim_end_matches(['/', '\\']) == norm)
        {
            return Err(SourceError::Validation(format!(
                "project already exists for {trimmed}"
            )));
        }
        let display = std::path::Path::new(trimmed)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| trimmed.to_string());
        // Synthesise a stable id so the App can pre-position the
        // sidebar cursor immediately. `mock-proj-N` keeps the shape
        // distinct from session ids (`mock-N`) so test assertions can
        // tell which call minted which id at a glance.
        self.next_project_id += 1;
        let id = format!("mock-proj-{}", self.next_project_id);
        self.projects.push(ProjectMeta {
            project_id: id.clone(),
            display_name: display,
            root_path: trimmed.to_string(),
        });
        Ok(ProjectId(id))
    }

    fn create_session(&mut self, req: NewSessionRequest) -> Result<SessionId, SourceError> {
        // Caller-side validation mirrors what the App enforces — keep
        // the mock honest so a unit test that drives the modal hits the
        // same gate the production source would.
        if req.backend.trim().is_empty() {
            return Err(SourceError::Validation("backend is required".into()));
        }
        // Resolve the project by either its registered id OR its root
        // path (the modal passes `project_dir` from the sidebar's
        // `root_path`). Falling back to the id keeps `MockSessionSource`
        // tests that pre-register a project with `add_project(id, …)`
        // working without forcing them to thread a path.
        let project_id = self
            .projects
            .iter()
            .find(|p| p.root_path == req.project_dir || p.project_id == req.project_dir)
            .map(|p| p.project_id.clone())
            .ok_or_else(|| {
                SourceError::Validation(format!("no project registered for {}", req.project_dir))
            })?;
        self.next_create_id += 1;
        let session_id = format!("mock-{}", self.next_create_id);
        let row = SessionRow {
            session_id: session_id.clone(),
            project_id,
            backend: Backend::new(&req.backend),
            title: None,
            run_state: RunState::Running,
            archived: false,
            discovered: false,
        };
        self.rows.insert(session_id.clone(), row);
        self.created.push(req);
        Ok(SessionId(session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_snapshot_shape() {
        let src = MockSessionSource::fixture();
        let snap = src.snapshot();
        assert_eq!(snap.len(), 4, "two projects + discovered + archived bucket");
        assert!(!snap[0].is_archived);
        assert!(snap.last().unwrap().is_archived, "archived pinned last");
        let discovered = snap
            .iter()
            .find(|g| g.is_discovered())
            .expect("discovered bucket exists");
        assert_eq!(discovered.sessions.len(), 2);
        assert!(discovered.sessions.iter().all(|s| s.discovered));
        assert_eq!(snap[0].sessions.len(), 2);
        assert_eq!(snap[1].sessions.len(), 2);
        assert_eq!(snap.last().unwrap().sessions.len(), 1);
    }

    #[test]
    fn import_discovered_routes_row_back_under_its_project() {
        let mut src = MockSessionSource::new();
        src.add_project("p1", "p1", "/p1");
        src.add_discovered_session("ext-1", "p1", "claude", Some("From disk"));
        let before = src.snapshot();
        assert!(before.iter().any(|g| g.is_discovered()));
        assert!(before
            .iter()
            .find(|g| g.project_id == "p1")
            .map(|g| g.sessions.is_empty())
            .unwrap_or(false));
        src.import_discovered("ext-1");
        let after = src.snapshot();
        assert!(
            after.iter().all(|g| !g.is_discovered()),
            "discovered bucket empties after import"
        );
        let p1 = after.iter().find(|g| g.project_id == "p1").unwrap();
        assert_eq!(p1.sessions.len(), 1);
        assert!(!p1.sessions[0].discovered);
    }

    #[test]
    fn archived_bucket_omitted_when_empty() {
        let mut src = MockSessionSource::new();
        src.add_project("p1", "p1", "/p1");
        src.add_session("s1", "p1", "claude", None, RunState::Idle);
        let snap = src.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(!snap[0].is_archived);
    }

    #[test]
    fn archive_then_restore_roundtrip() {
        let mut src = MockSessionSource::new();
        src.add_project("p1", "p1", "/p1");
        src.add_session("s1", "p1", "claude", None, RunState::Idle);
        src.archive("s1");
        assert!(src.snapshot()[0].is_archived || src.snapshot().len() == 2);
        // Confirm row landed in the archived bucket.
        let snap = src.snapshot();
        let archived = snap.iter().find(|g| g.is_archived).expect("bucket exists");
        assert_eq!(archived.sessions.len(), 1);
        src.restore("s1");
        let snap2 = src.snapshot();
        assert!(snap2.iter().all(|g| !g.is_archived), "bucket emptied");
    }

    #[test]
    fn delete_removes_row() {
        let mut src = MockSessionSource::new();
        src.add_project("p1", "p1", "/p1");
        src.add_session("s1", "p1", "claude", None, RunState::Idle);
        src.delete("s1");
        assert_eq!(src.snapshot()[0].sessions.len(), 0);
    }

    #[test]
    fn create_session_inserts_row_under_resolved_project() {
        let mut src = MockSessionSource::new();
        src.add_project("proj-a", "proj-a", "/home/me/code/proj-a");
        let id = src
            .create_session(NewSessionRequest {
                project_dir: "/home/me/code/proj-a".into(),
                backend: "claude".into(),
                args: Vec::new(),
                worktree: false,
            })
            .expect("create ok");
        assert!(id.as_str().starts_with("mock-"));
        let snap = src.snapshot();
        let proj = snap
            .iter()
            .find(|g| g.project_id == "proj-a")
            .expect("project group");
        assert_eq!(proj.sessions.len(), 1);
        assert_eq!(proj.sessions[0].session_id, id.as_str());
        assert_eq!(proj.sessions[0].backend.id(), "claude");
        assert_eq!(proj.sessions[0].title, None);
        // create() also resolves a project by its registered id, so a
        // caller threading the project_id (as the App does when the
        // sidebar root_path is empty) still works.
        let id2 = src
            .create_session(NewSessionRequest {
                project_dir: "proj-a".into(),
                backend: "codex".into(),
                args: Vec::new(),
                worktree: true,
            })
            .expect("create ok via project id");
        assert_eq!(id2.as_str(), "mock-2");
        assert_eq!(src.created().len(), 2);
        assert!(src.created()[1].worktree);
    }

    #[test]
    fn create_session_rejects_unknown_project() {
        let mut src = MockSessionSource::new();
        src.add_project("proj-a", "proj-a", "/p/a");
        let unknown = src.create_session(NewSessionRequest {
            project_dir: "/nope".into(),
            backend: "claude".into(),
            args: Vec::new(),
            worktree: false,
        });
        assert!(matches!(unknown, Err(SourceError::Validation(_))));
        // Nothing was inserted.
        assert!(src.created().is_empty());
        assert_eq!(src.snapshot()[0].sessions.len(), 0);
    }

    #[test]
    fn create_project_accepts_two_distinct_paths_with_same_basename() {
        // Code Reviewer's blocker on PR #92: dedup must be by
        // `root_path` only — never by basename — so a user with
        // `/repo/app` can still register `/tmp/app`. The daemon's
        // `projects` table itself only enforces uniqueness on
        // `root_path` (la-storage migration 0001).
        let mut src = MockSessionSource::new();
        let first = src
            .create_project("/repo/app")
            .expect("first /repo/app succeeds");
        let second = src
            .create_project("/tmp/app")
            .expect("same basename, different path also succeeds");
        assert_ne!(first.as_str(), second.as_str(), "distinct project ids");
        let snap = src.snapshot();
        let roots: Vec<&str> = snap.iter().map(|g| g.root_path.as_str()).collect();
        assert!(
            roots.contains(&"/repo/app") && roots.contains(&"/tmp/app"),
            "both projects must be present in the snapshot; got {roots:?}"
        );
        // ... but a literal repeat of the same path is still rejected.
        let dup = src.create_project("/repo/app");
        assert!(matches!(dup, Err(SourceError::Validation(_))));
    }

    #[test]
    fn create_project_trailing_slash_normalizes_against_existing() {
        // Trailing-slash variants are the only path normalization the
        // dedup performs (matching the daemon's `ensure_project`, which
        // compares literal strings). `/p/a/` and `/p/a` should collide
        // so we don't accidentally seed two stubs for the same dir.
        let mut src = MockSessionSource::new();
        src.create_project("/p/a").expect("first succeeds");
        let dup = src.create_project("/p/a/");
        assert!(
            matches!(dup, Err(SourceError::Validation(_))),
            "trailing slash variant must dedupe, got {dup:?}"
        );
    }
}
