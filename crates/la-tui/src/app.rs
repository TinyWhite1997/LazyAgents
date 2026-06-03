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

use crate::crons::{CronSource, CronsState, FieldEdit};
use crate::model::BackendBadge;
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
///
/// The Crons tab (M3.4) reuses the same enum: `Sidebar` == cron list,
/// `Main` == editor form. Keeping the two tabs on the same Focus state
/// is enough because at any moment the user is on exactly one tab.
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
    /// `space` on a disabled cron, first enable only (WEK-35 / M3.4):
    /// shows the cost budget plus next trigger time and forces an
    /// explicit confirmation before the daemon picks the cron up. The
    /// fields are pre-computed by the App so the renderer can stay
    /// declarative.
    ConfirmEnableCron {
        cron_id: String,
        cron_name: String,
        budget_label: String,
        next_label: String,
    },
    /// `d` on a cron row → confirm delete. We do NOT reuse
    /// `ConfirmDelete` to avoid the input router routing the wrong
    /// source.delete()/cron source.delete() call.
    ConfirmDeleteCron { cron_id: String, cron_name: String },
    /// `R` on a cron row → list the next 5 fire times so the user can
    /// sanity-check the expression before enabling. Read-only modal.
    DryRunCron { cron_id: String, fires: Vec<String> },
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
    /// `i` promote a Discovered session into a native row via
    /// `sessions.import` (WEK-26 / M2.3). No-op outside the
    /// Discovered bucket.
    ImportDiscovered,
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
    /// Replace the backend health snapshot (driven by `daemon.health`
    /// notifications — WEK-29). The TUI consumes this to grey-state
    /// unavailable backends in the dedicated Backends panel.
    BackendsUpdate(Vec<BackendBadge>),

    // --- Crons tab (WEK-35 / M3.4) ---------------------------------
    //
    // The Crons tab has its own focus model: when the user is in the
    // cron list these messages drive list navigation; once focus is on
    // the editor pane they drive the per-field buffer instead. The App
    // gates routing on `self.focus` so the input layer doesn't have to
    // know the difference.
    CronListDown,
    CronListUp,
    CronListTop,
    CronListBottom,
    /// `n` — start a new cron in the editor pane.
    CronNew,
    /// `d` — request delete confirmation for the selected cron.
    CronDelete,
    /// `space` — toggle enabled. Opens the first-enable confirm modal
    /// when the transition is "disabled → enabled".
    CronToggleEnabled,
    /// `r` — fire the selected cron once. No-op if the list is empty.
    CronTriggerNow,
    /// `R` — open the dry-run modal showing the next 5 fire times.
    CronDryRun,
    /// Move focus into the editor pane (Tab / `i`) or back to the list
    /// (Esc with no draft). Idempotent.
    CronFocusEditor,
    CronFocusList,
    /// Switch the field cursor inside the editor (Tab / Shift-Tab).
    CronFieldNext,
    CronFieldPrev,
    /// One keystroke into the active field.
    CronFieldEdit(FieldEdit),
    /// `Ctrl+S` from list or editor — commit the in-flight draft.
    CronSaveDraft,
    /// `Enter` from the editor pane. The App decides at dispatch time
    /// whether to insert a newline (multi-line field) or save the draft
    /// (single-line field) — that keeps the policy in one place instead
    /// of forcing the input layer to know the active field.
    CronEditorEnter,
    /// `Esc` from the editor with a draft open — discard it.
    CronCancelDraft,
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
pub struct App<S: SessionSource, C: CronSource = crate::crons::MockCronSource> {
    source: S,
    cron_source: C,
    pub sidebar: SidebarState,
    pub crons: CronsState,
    pub tab: Tab,
    pub focus: Focus,
    pub modal: Option<Modal>,
    pub status: Status,
    /// Latest snapshot of every registered backend's probe state
    /// (architecture §4.3 / WEK-29). Empty until the first
    /// `daemon.health` arrives. The rendering layer reads it to display
    /// the Backends panel above the project list.
    pub backends: Vec<BackendBadge>,
    /// Set by the App whenever a user-driven side effect needs to
    /// surface a toast to the runner ("triggered now", "saved", "no
    /// changes to save"). The runner is free to ignore it — it exists
    /// to keep tests deterministic without poking at private state.
    pub last_toast: Option<String>,
}

impl<S: SessionSource> App<S, crate::crons::MockCronSource> {
    /// Convenience constructor that pairs an arbitrary session source
    /// with the in-memory mock cron source. Live wiring will swap in an
    /// IPC-backed cron source via [`App::with_sources`] (M3.5).
    pub fn new(source: S) -> Self {
        Self::with_sources(source, crate::crons::MockCronSource::fixture())
    }
}

impl<S: SessionSource, C: CronSource> App<S, C> {
    /// Construct with the given source. Loads an initial snapshot into
    /// the sidebar so the very first frame shows real data instead of an
    /// empty pane.
    pub fn with_sources(source: S, cron_source: C) -> Self {
        let mut crons = CronsState::new();
        crons.set_crons(cron_source.snapshot());
        let mut app = Self {
            source,
            cron_source,
            sidebar: SidebarState::new(),
            crons,
            tab: Tab::Sessions,
            focus: Focus::Sidebar,
            modal: None,
            status: Status {
                daemon_online: false,
                running: 0,
                next_cron_label: None,
                right_context: String::new(),
            },
            backends: Vec::new(),
            last_toast: None,
        };
        app.refresh_sessions();
        app
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    pub fn cron_source(&self) -> &C {
        &self.cron_source
    }

    /// Refresh the cron snapshot from the source (after an upsert/delete).
    pub fn refresh_crons(&mut self) {
        self.crons.set_crons(self.cron_source.snapshot());
    }

    /// Pull a fresh snapshot from the source and hand it to the sidebar.
    /// The sidebar preserves selection + expanded flags across the swap.
    pub fn refresh_sessions(&mut self) {
        self.sidebar.set_groups(self.source.snapshot());
    }

    /// Apply one input message and return whether the runner should
    /// continue.
    pub fn handle(&mut self, msg: AppMsg) -> AppOutcome {
        // Per-message toast slate: every dispatch starts clean so a
        // surviving toast from a previous frame can't leak forward.
        self.last_toast = None;
        // Modal short-circuits — while a modal is open, only its keys are
        // valid (PRD §5.6 上下文驱动).
        if let Some(modal) = self.modal.clone() {
            return self.handle_in_modal(modal, msg);
        }
        match msg {
            AppMsg::Quit => return AppOutcome::Quit,
            AppMsg::NextTab => {
                self.tab = self.tab.next();
                self.focus = Focus::Sidebar;
            }
            AppMsg::PrevTab => {
                self.tab = self.tab.prev();
                self.focus = Focus::Sidebar;
            }
            AppMsg::SetTab(t) => {
                self.tab = t;
                self.focus = Focus::Sidebar;
            }
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
            AppMsg::ImportDiscovered => self.on_import_discovered(),
            AppMsg::Confirm | AppMsg::Cancel => {
                // Outside a modal: `Esc` on the Crons editor pane drops
                // the user back to the list (and the draft, if any).
                if matches!(msg, AppMsg::Cancel)
                    && self.tab == Tab::Crons
                    && self.focus == Focus::Main
                {
                    if self.crons.cancel_edit() {
                        self.last_toast = Some("draft discarded".into());
                    }
                    self.focus = Focus::Sidebar;
                }
            }
            AppMsg::ToggleFullHints => {
                self.modal = Some(Modal::FullHints);
            }
            AppMsg::StatusUpdate(s) => self.status = s,
            AppMsg::RefreshSessions => self.refresh_sessions(),
            AppMsg::BackendsUpdate(b) => {
                // Sort by id so the rendered order is stable across
                // pulses (the daemon already sorts, but a paranoid TUI
                // can't hurt).
                let mut sorted = b;
                sorted.sort_by(|a, b| a.id.cmp(&b.id));
                self.backends = sorted;
            }

            // --- Crons tab dispatch ---------------------------------
            AppMsg::CronListDown => {
                self.crons.move_cursor(1);
            }
            AppMsg::CronListUp => {
                self.crons.move_cursor(-1);
            }
            AppMsg::CronListTop => self.crons.move_top(),
            AppMsg::CronListBottom => self.crons.move_bottom(),
            AppMsg::CronNew => self.on_cron_new(),
            AppMsg::CronDelete => self.on_cron_delete(),
            AppMsg::CronToggleEnabled => self.on_cron_toggle(),
            AppMsg::CronTriggerNow => self.on_cron_trigger_now(),
            AppMsg::CronDryRun => self.on_cron_dry_run(),
            AppMsg::CronFocusEditor => {
                if self.tab == Tab::Crons && self.crons.selected().is_some() {
                    self.focus = Focus::Main;
                }
            }
            AppMsg::CronFocusList => {
                if self.tab == Tab::Crons {
                    self.focus = Focus::Sidebar;
                }
            }
            AppMsg::CronFieldNext => self.crons.field_next(),
            AppMsg::CronFieldPrev => self.crons.field_prev(),
            AppMsg::CronFieldEdit(edit) => {
                self.crons.field_input(edit);
            }
            AppMsg::CronSaveDraft => self.on_cron_save(),
            AppMsg::CronEditorEnter => {
                // Multi-line field → insert a newline so prompts and
                // arg lists actually accept multiple rows. Single-line
                // field → save, matching the muscle memory the user
                // already has from the list pane's "Enter = primary".
                if self.crons.field().is_multiline() {
                    self.crons
                        .field_input(crate::crons::FieldEdit::InsertNewline);
                } else {
                    self.on_cron_save();
                }
            }
            AppMsg::CronCancelDraft => {
                if self.crons.cancel_edit() {
                    self.last_toast = Some("draft discarded".into());
                }
                self.focus = Focus::Sidebar;
            }
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
            (Modal::FullHints, AppMsg::Cancel) | (Modal::FullHints, AppMsg::ToggleFullHints) => {
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
            (Modal::ConfirmEnableCron { cron_id, .. }, AppMsg::Confirm) => {
                self.cron_source.set_enabled(&cron_id, true);
                self.modal = None;
                self.refresh_crons();
                self.last_toast = Some("cron enabled".into());
            }
            (Modal::ConfirmEnableCron { cron_id, .. }, AppMsg::Cancel) => {
                // User backed out — flip the in-memory state back to
                // disabled so the list reflects what's actually on the
                // daemon.
                self.cron_source.set_enabled(&cron_id, false);
                self.modal = None;
                self.refresh_crons();
            }
            (Modal::ConfirmDeleteCron { cron_id, .. }, AppMsg::Confirm) => {
                self.cron_source.delete(&cron_id);
                self.modal = None;
                self.refresh_crons();
                self.last_toast = Some("cron deleted".into());
            }
            (Modal::ConfirmDeleteCron { .. }, AppMsg::Cancel) => {
                self.modal = None;
            }
            (Modal::DryRunCron { .. }, AppMsg::Cancel)
            | (Modal::DryRunCron { .. }, AppMsg::Confirm) => {
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

    /// `i` on a Discovered row: hand the row's external id to the
    /// source so it can call `sessions.import` (or, in the mock,
    /// flip the discovered flag). No-op on any other row so the key
    /// stays silent outside the bucket.
    fn on_import_discovered(&mut self) {
        let sel = self.sidebar.selection();
        let Some(sid) = sel.session_id() else { return };
        let is_discovered = self
            .sidebar
            .groups()
            .iter()
            .flat_map(|g| g.sessions.iter())
            .find(|s| s.session_id == sid)
            .map(|s| s.discovered)
            .unwrap_or(false);
        if !is_discovered {
            return;
        }
        let sid = sid.to_string();
        self.source.import_discovered(&sid);
        self.refresh_sessions();
    }

    // -----------------------------------------------------------------
    // Crons tab handlers (WEK-35 / M3.4)
    //
    // Each helper translates one user-visible intent into a single
    // CronSource call plus a snapshot refresh. The handlers consciously
    // refuse to do work when there's no selection (`r`/`d`/`space` on an
    // empty list are silent), matching the "hint == 真实绑定" rule from
    // the Sessions tab.
    // -----------------------------------------------------------------

    fn on_cron_new(&mut self) {
        // Use a UUIDv7-shaped placeholder so the editor's "name" buffer
        // has something to render. The id field is hidden from the UI;
        // the live source rewrites it on `upsert`.
        let temp_id = format!("draft-{}", self.crons.crons().len() + 1);
        self.crons.begin_new(temp_id);
        self.focus = Focus::Main;
    }

    fn on_cron_delete(&mut self) {
        let Some(cron) = self.crons.selected() else {
            return;
        };
        self.modal = Some(Modal::ConfirmDeleteCron {
            cron_id: cron.id.clone(),
            cron_name: cron.name.clone(),
        });
    }

    fn on_cron_toggle(&mut self) {
        // Special case: editor focus + draft means the user pressed
        // space while typing into the prompt buffer — route to the
        // buffer instead of toggling.
        if self.focus == Focus::Main && self.crons.draft().is_some() {
            self.crons.field_input(FieldEdit::Insert(' '));
            return;
        }
        let Some((id, now_enabled)) = self.crons.toggle_enabled() else {
            return;
        };
        if now_enabled {
            // First-enable confirmation: pre-compute the cost-budget and
            // next-fire labels so the renderer can stay declarative.
            let cron = match self.crons.crons().iter().find(|c| c.id == id) {
                Some(c) => c.clone(),
                None => return,
            };
            let budget_label = cron
                .cost_budget_usd_per_day
                .map(|v| format!("${v:.2}/day"))
                .unwrap_or_else(|| "inherits global default".to_string());
            // Use the (already-refreshed) preview so the modal label is
            // byte-identical to the inline "下次：…" hint the user just
            // saw — no risk of the two disagreeing.
            let next_label = match self.crons.preview().next {
                Some(n) => crate::crons::human_label(n, self.crons.now(), &cron.tz),
                None => "下次：—".to_string(),
            };
            self.modal = Some(Modal::ConfirmEnableCron {
                cron_id: id,
                cron_name: cron.name,
                budget_label,
                next_label,
            });
        } else {
            // Disabling is one-step; persist immediately.
            self.cron_source.set_enabled(&id, false);
            self.refresh_crons();
            self.last_toast = Some("cron disabled".into());
        }
    }

    fn on_cron_trigger_now(&mut self) {
        let Some(id) = self.crons.selected_id().map(str::to_string) else {
            return;
        };
        self.cron_source.trigger_now(&id);
        self.last_toast = Some(format!("triggered {id}"));
    }

    fn on_cron_dry_run(&mut self) {
        let Some(cron) = self.crons.selected().cloned() else {
            return;
        };
        // Use the editor's preview directly — same `cron::Schedule` call
        // the scheduler will use, per the WEK-35 acceptance.
        let preview = self.crons.preview();
        let fires = if preview.error.is_some() {
            vec![format!(
                "invalid: {}",
                preview.error.as_deref().unwrap_or("?")
            )]
        } else {
            preview
                .all_fires()
                .into_iter()
                .map(|t| t.with_timezone(&parse_tz(&cron.tz)).to_rfc3339())
                .collect()
        };
        self.modal = Some(Modal::DryRunCron {
            cron_id: cron.id.clone(),
            fires,
        });
    }

    fn on_cron_save(&mut self) {
        let Some(draft) = self.crons.draft().cloned() else {
            self.last_toast = Some("no draft to save".into());
            return;
        };
        // Validate first — the optimistic local commit must NOT happen
        // for a draft the daemon will refuse. Reuse the editor's
        // already-computed preview rather than re-parsing.
        let preview =
            crate::crons::CronPreview::compute(&draft.cron_expr, &draft.tz, self.crons.now());
        if preview.error.is_some() {
            self.last_toast = Some(format!(
                "save aborted: {}",
                preview.error.as_deref().unwrap_or("invalid cron")
            ));
            return;
        }
        // Commit + persist. `commit_draft` clears the in-flight draft and
        // returns the committed cron so we can hand it to the source.
        let committed = self
            .crons
            .commit_draft()
            .expect("draft was Some moments ago");
        // TODO(M3.5): `CronSource::upsert` will become an IPC round-trip
        // that returns the daemon-assigned UUID — `commit_draft`'s
        // optimistic local push uses a `draft-N` placeholder id, so
        // `set_crons` (via `refresh_crons`) loses the cursor on insert
        // because the post-save snapshot keys the row under a new id.
        // Plan for M3.5: make the trait return `Cron`, reseed the cursor
        // from the response. Mock today preserves the id so the unit
        // tests stay deterministic.
        self.cron_source.upsert(committed);
        self.refresh_crons();
        self.focus = Focus::Sidebar;
        self.last_toast = Some("saved".into());
    }
}

/// Resolve an IANA tz string for the dry-run RFC3339 rendering. Falls
/// back to UTC on a bad spec — the modal is informational only and a
/// bad tz means we already showed an error in the editor.
fn parse_tz(s: &str) -> chrono_tz::Tz {
    s.parse().unwrap_or(chrono_tz::UTC)
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
        assert!(
            a.modal.is_none(),
            "no new-session chooser on archived bucket"
        );
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

    #[test]
    fn i_on_discovered_row_promotes_it_into_its_project() {
        let mut a = app();
        // Navigate cursor into the Discovered bucket and pick its first
        // row.
        a.handle(AppMsg::SidebarBottom); // archived (or last) group header
                                         // Walk up until we land inside the Discovered group — fixture
                                         // places it just above Archived.
        loop {
            a.handle(AppMsg::SidebarUp);
            let in_discovered = a
                .sidebar
                .selection()
                .session_id()
                .and_then(|sid| {
                    a.sidebar
                        .groups()
                        .iter()
                        .flat_map(|g| g.sessions.iter())
                        .find(|s| s.session_id == sid)
                })
                .map(|s| s.discovered)
                .unwrap_or(false);
            if in_discovered {
                break;
            }
            // Safety: stop if we accidentally reach the top — the fixture
            // is small enough this should never fire, and asserting keeps
            // the test honest about its assumption.
            if matches!(a.sidebar.selection(), Selection::Empty) {
                panic!("could not find a discovered row");
            }
        }
        let sid = a
            .sidebar
            .selection()
            .session_id()
            .map(str::to_string)
            .unwrap();
        a.handle(AppMsg::ImportDiscovered);
        let snap = a.source().snapshot();
        let still_discovered = snap
            .iter()
            .filter(|g| g.is_discovered())
            .flat_map(|g| g.sessions.iter())
            .any(|s| s.session_id == sid);
        assert!(!still_discovered, "row left the Discovered bucket");
        let in_project = snap
            .iter()
            .filter(|g| !g.is_discovered() && !g.is_archived)
            .flat_map(|g| g.sessions.iter())
            .any(|s| s.session_id == sid);
        assert!(in_project, "row landed under its real project");
    }
}
