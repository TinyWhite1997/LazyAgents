//! Top-level App state.
//!
//! The App holds:
//! - which tab is active (Sessions / Crons)
//! - which pane has focus (sidebar / main, etc.)
//! - the sidebar navigation state ([`crate::sidebar::SidebarState`])
//! - any open modal (delete confirmation, full hints overlay, new-session
//!   chooser)
//! - a status snapshot ([`crate::status::Status`]) that the daemon-driven
//!   binary refreshes from `events.subscribe`
//!
//! Input is funneled through [`App::handle`], which takes a high-level
//! [`AppMsg`] (already translated from a raw `crossterm::event::Event`
//! by [`crate::input`]). This decoupling makes the App testable without
//! a terminal.

use crate::sidebar::{Selection, SidebarState};
use crate::source::SessionSource;
use crate::status::Status;

/// Top-level tabs (PRD §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Sessions,
    Crons,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Sessions, Tab::Crons];
    pub fn label(self) -> &'static str {
        match self {
            Tab::Sessions => "Sessions",
            Tab::Crons => "Crons",
        }
    }
    fn next(self) -> Tab {
        match self {
            Tab::Sessions => Tab::Crons,
            Tab::Crons => Tab::Sessions,
        }
    }
    fn prev(self) -> Tab {
        self.next() // 2 tabs ⇒ next == prev
    }
}

/// Which pane currently owns focus. M1.5 only has Sidebar; Main is
/// reserved for the conversation pane that M1.6 will add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
}

/// Modal overlays. At most one open at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// `d` on a session row → confirm hard-delete (PRD §5.3 二次确认).
    ConfirmDelete { session_id: String },
    /// `?` opens the full which-key overlay.
    FullHints,
    /// `n` on a project → backend chooser (placeholder until M1.7 wires
    /// `sessions.create`). The actual chooser UI lands with the daemon; for
    /// M1.5 we just acknowledge the request so the key path is visible.
    NewSession { project_id: String },
}

/// All input the App reacts to. The input layer translates raw events
/// (key, mouse) into these high-level intents; the App does NOT touch
/// crossterm types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMsg {
    /// User asked to quit. The runner exits the event loop.
    Quit,
    /// Cycle to the next or previous tab.
    NextTab,
    PrevTab,
    /// Jump to a specific tab (digit shortcut or mouse click).
    SetTab(Tab),
    /// Sidebar navigation.
    SidebarDown,
    SidebarUp,
    SidebarTop,
    SidebarBottom,
    /// Collapse / expand the current group.
    SidebarCollapse,
    SidebarExpand,
    /// Jump cursor to a flat-list index (mouse click on a list row).
    SidebarSelect(usize),
    /// Primary action on the current selection.
    Enter,
    /// `d` (with confirmation).
    Delete,
    /// `a` archive (instant) or restore (when cursor is on an archived
    /// session).
    ArchiveOrRestore,
    /// `n` new session (opens chooser modal).
    NewSession,
    /// Confirm / cancel inside a modal.
    Confirm,
    Cancel,
    /// `?` toggles the which-key overlay.
    ToggleFullHints,
    /// Status pushed from the IPC layer (cron preview, running count, …).
    StatusUpdate(Status),
    /// Replace the sidebar snapshot (called whenever the source's data
    /// changes — initial load, archive event, etc.).
    RefreshSessions,
}

/// What the runner should do after a message has been handled.
///
/// `Continue` is the normal case; `Quit` tells the event loop to exit
/// after the next render flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppOutcome {
    Continue,
    Quit,
}

/// Top-level application.
pub struct App<S: SessionSource> {
    source: S,
    pub sidebar: SidebarState,
    pub tab: Tab,
    pub focus: Focus,
    pub modal: Option<Modal>,
    pub status: Status,
}

impl<S: SessionSource> App<S> {
    /// Construct with the given source. Loads an initial snapshot into
    /// the sidebar so the very first frame shows real data instead of an
    /// empty pane.
    pub fn new(source: S) -> Self {
        let mut app = Self {
            source,
            sidebar: SidebarState::new(),
            tab: Tab::Sessions,
            focus: Focus::Sidebar,
            modal: None,
            status: Status {
                daemon_online: false,
                running: 0,
                next_cron_label: None,
                right_context: String::new(),
            },
        };
        app.refresh_sessions();
        app
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// Pull a fresh snapshot from the source and hand it to the sidebar.
    /// The sidebar preserves selection + expanded flags across the swap.
    pub fn refresh_sessions(&mut self) {
        self.sidebar.set_groups(self.source.snapshot());
    }

    /// Apply one input message and return whether the runner should
    /// continue.
    pub fn handle(&mut self, msg: AppMsg) -> AppOutcome {
        // Modal short-circuits — while a modal is open, only its keys are
        // valid (PRD §5.6 上下文驱动).
        if let Some(modal) = self.modal.clone() {
            return self.handle_in_modal(modal, msg);
        }
        match msg {
            AppMsg::Quit => return AppOutcome::Quit,
            AppMsg::NextTab => self.tab = self.tab.next(),
            AppMsg::PrevTab => self.tab = self.tab.prev(),
            AppMsg::SetTab(t) => self.tab = t,
            AppMsg::SidebarDown => {
                self.sidebar.move_down();
            }
            AppMsg::SidebarUp => {
                self.sidebar.move_up();
            }
            AppMsg::SidebarTop => {
                self.sidebar.move_top();
            }
            AppMsg::SidebarBottom => {
                self.sidebar.move_bottom();
            }
            AppMsg::SidebarCollapse => {
                self.sidebar.collapse();
            }
            AppMsg::SidebarExpand => {
                self.sidebar.expand();
            }
            AppMsg::SidebarSelect(i) => {
                self.sidebar.select_index(i);
            }
            AppMsg::Enter => self.on_enter(),
            AppMsg::Delete => self.on_delete(),
            AppMsg::ArchiveOrRestore => self.on_archive_or_restore(),
            AppMsg::NewSession => self.on_new_session(),
            AppMsg::Confirm | AppMsg::Cancel => {
                // No-op outside a modal.
            }
            AppMsg::ToggleFullHints => {
                self.modal = Some(Modal::FullHints);
            }
            AppMsg::StatusUpdate(s) => self.status = s,
            AppMsg::RefreshSessions => self.refresh_sessions(),
        }
        AppOutcome::Continue
    }

    fn handle_in_modal(&mut self, modal: Modal, msg: AppMsg) -> AppOutcome {
        match (modal, msg) {
            (Modal::ConfirmDelete { session_id }, AppMsg::Confirm) => {
                self.source.delete(&session_id);
                self.modal = None;
                self.refresh_sessions();
            }
            (Modal::ConfirmDelete { .. }, AppMsg::Cancel) => {
                self.modal = None;
            }
            (Modal::FullHints, AppMsg::Cancel)
            | (Modal::FullHints, AppMsg::ToggleFullHints) => {
                self.modal = None;
            }
            (Modal::NewSession { .. }, AppMsg::Confirm) => {
                // M1.5 cannot actually create a session (no daemon yet).
                // Close the modal so the key path is at least visible.
                self.modal = None;
            }
            (Modal::NewSession { .. }, AppMsg::Cancel) => {
                self.modal = None;
            }
            // Inside a modal, Quit still wins (so the user is never
            // stranded if a modal refuses to close).
            (_, AppMsg::Quit) => return AppOutcome::Quit,
            _ => {}
        }
        AppOutcome::Continue
    }

    fn on_enter(&mut self) {
        // M1.5: entering a session does not yet open a conversation pane
        // (that's M1.6); we leave the hook here so the binding is wired and
        // the key bar can claim "⏎ open" correctly. The actual `sessions.
        // attach` call will live in this branch once la-core lands.
        if let Selection::Group { .. } = self.sidebar.selection() {
            // On a group header, Enter toggles fold/expand for parity with
            // file-tree muscle memory.
            let was_expanded = self
                .sidebar
                .groups()
                .iter()
                .find(|g| Some(g.project_id.as_str()) == self.sidebar.selection().project_id())
                .map(|g| g.expanded)
                .unwrap_or(true);
            if was_expanded {
                self.sidebar.collapse();
            } else {
                self.sidebar.expand();
            }
        }
        // Session selection: actual attach is a TODO for M1.6 + M1.7.
    }

    fn on_delete(&mut self) {
        if let Some(sid) = self.sidebar.selection().session_id() {
            self.modal = Some(Modal::ConfirmDelete {
                session_id: sid.to_string(),
            });
        }
    }

    fn on_archive_or_restore(&mut self) {
        let sel = self.sidebar.selection();
        let Some(sid) = sel.session_id() else { return };
        // Look up the row to decide archive vs restore — "a" on an
        // already-archived session restores it (PRD §5.3 "可展开恢复").
        let is_archived = self
            .sidebar
            .groups()
            .iter()
            .flat_map(|g| g.sessions.iter())
            .find(|s| s.session_id == sid)
            .map(|s| s.archived)
            .unwrap_or(false);
        if is_archived {
            self.source.restore(sid);
        } else {
            self.source.archive(sid);
        }
        self.refresh_sessions();
    }

    fn on_new_session(&mut self) {
        let project_id = match self.sidebar.selection() {
            Selection::Group { project_id } => project_id,
            Selection::Session { project_id, .. } => project_id,
            Selection::Empty => return,
        };
        // No new sessions in the synthetic Archived bucket.
        if project_id == crate::model::ProjectGroup::ARCHIVED_ID {
            return;
        }
        self.modal = Some(Modal::NewSession { project_id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MockSessionSource;

    fn app() -> App<MockSessionSource> {
        App::new(MockSessionSource::fixture())
    }

    #[test]
    fn jk_drives_sidebar_when_focused() {
        let mut a = app();
        a.handle(AppMsg::SidebarDown);
        assert!(matches!(a.sidebar.selection(), Selection::Session { .. }));
    }

    #[test]
    fn d_opens_confirm_modal_and_y_deletes() {
        let mut a = app();
        a.handle(AppMsg::SidebarDown); // first session
        a.handle(AppMsg::Delete);
        let Some(Modal::ConfirmDelete { ref session_id }) = a.modal else {
            panic!("expected confirm-delete modal");
        };
        let sid = session_id.clone();
        a.handle(AppMsg::Confirm);
        assert!(a.modal.is_none(), "modal closed");
        // Row gone from the snapshot.
        let snap = a.source().snapshot();
        let still_there = snap
            .iter()
            .flat_map(|g| g.sessions.iter())
            .any(|s| s.session_id == sid);
        assert!(!still_there);
    }

    #[test]
    fn d_cancel_keeps_session() {
        let mut a = app();
        a.handle(AppMsg::SidebarDown);
        a.handle(AppMsg::Delete);
        a.handle(AppMsg::Cancel);
        assert!(a.modal.is_none());
        let snap = a.source().snapshot();
        let total: usize = snap.iter().map(|g| g.sessions.len()).sum();
        assert!(total > 0);
    }

    #[test]
    fn a_archives_then_restores_the_same_session() {
        let mut a = app();
        // Move to a non-archived session.
        a.handle(AppMsg::SidebarDown);
        let sid_before = a.sidebar.selection().session_id().unwrap().to_string();
        a.handle(AppMsg::ArchiveOrRestore);
        // Row should now be in the Archived bucket.
        let snap = a.source().snapshot();
        let in_archived = snap
            .iter()
            .find(|g| g.is_archived)
            .map(|g| g.sessions.iter().any(|s| s.session_id == sid_before))
            .unwrap_or(false);
        assert!(in_archived, "session moved to Archived bucket");
        // Jump to it in the Archived bucket and restore.
        a.handle(AppMsg::SidebarBottom); // archived header
        a.handle(AppMsg::SidebarExpand);
        a.handle(AppMsg::SidebarDown); // first archived row
        // It may not be sid_before (sort order), but ArchiveOrRestore on
        // any archived row should set archived=false.
        let sid_arch = a
            .sidebar
            .selection()
            .session_id()
            .map(str::to_string)
            .unwrap();
        a.handle(AppMsg::ArchiveOrRestore);
        let snap2 = a.source().snapshot();
        let in_archived2 = snap2
            .iter()
            .find(|g| g.is_archived)
            .map(|g| g.sessions.iter().any(|s| s.session_id == sid_arch))
            .unwrap_or(false);
        assert!(!in_archived2, "session restored out of Archived");
    }

    #[test]
    fn n_on_archived_bucket_does_nothing() {
        let mut a = app();
        a.handle(AppMsg::SidebarBottom); // archived header
        a.handle(AppMsg::NewSession);
        assert!(a.modal.is_none(), "no new-session chooser on archived bucket");
    }

    #[test]
    fn enter_toggles_group_header_folding() {
        let mut a = app();
        // First item is a group header.
        let was_expanded = a.sidebar.groups()[0].expanded;
        a.handle(AppMsg::Enter);
        assert_ne!(a.sidebar.groups()[0].expanded, was_expanded);
    }

    #[test]
    fn question_mark_opens_full_hints_and_closes() {
        let mut a = app();
        a.handle(AppMsg::ToggleFullHints);
        assert_eq!(a.modal, Some(Modal::FullHints));
        a.handle(AppMsg::ToggleFullHints);
        assert!(a.modal.is_none());
    }

    #[test]
    fn quit_inside_modal_still_quits() {
        let mut a = app();
        a.handle(AppMsg::SidebarDown);
        a.handle(AppMsg::Delete); // opens modal
        let outcome = a.handle(AppMsg::Quit);
        assert_eq!(outcome, AppOutcome::Quit);
    }

    #[test]
    fn tab_cycles() {
        let mut a = app();
        assert_eq!(a.tab, Tab::Sessions);
        a.handle(AppMsg::NextTab);
        assert_eq!(a.tab, Tab::Crons);
        a.handle(AppMsg::NextTab);
        assert_eq!(a.tab, Tab::Sessions);
    }

    #[test]
    fn status_update_replaces_status() {
        let mut a = app();
        let s = Status {
            daemon_online: true,
            running: 3,
            next_cron_label: Some("next 02:00".into()),
            right_context: "main · +12".into(),
        };
        a.handle(AppMsg::StatusUpdate(s.clone()));
        assert_eq!(a.status.running, 3);
        assert!(a.status.daemon_online);
        assert_eq!(a.status.next_cron_label.as_deref(), Some("next 02:00"));
    }
}
