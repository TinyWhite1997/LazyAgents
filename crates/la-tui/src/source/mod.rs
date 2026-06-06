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

use crate::model::{Backend, ProjectGroup, RunState, SessionRow};

pub mod rpc;
pub use rpc::RpcSessionSource;

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
        }
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
}
