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
use crate::notif_sub::HealthSnapshot;
use crate::sidebar::{Selection, SidebarState};
use crate::source::{NewSessionRequest, SessionSource, SourceError};
use crate::status::{CronPulse, Status};
use crate::ui_prefs::UiPrefs;
use la_proto::notifications::CronFiredParams;
use std::path::PathBuf;

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
///
/// WEK-92-A3 adds `Transcript`: when the Sessions tab has an active
/// attach, focus moves to the transcript / composer pane and keystrokes
/// route into the daemon via [`crate::attach_pump`]. Detach drops focus
/// back to `Sidebar` and clears the attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
    Transcript,
}

/// State carried inside [`Modal::NewSession`]. Holds the user's draft
/// across redraws so the input keys (Tab cycle, backend ←/→, prompt
/// typing, worktree toggle) can mutate it in place.
///
/// Kept as a plain struct of POD-ish fields so the parent [`Modal`] can
/// still derive `Clone` + `PartialEq` + `Eq` (the rest of the modal
/// machinery relies on that). The composer-style prompt buffer is a
/// `String` here rather than a full [`crate::composer::Composer`] —
/// A2's scope only needs append + backspace + Enter-for-newline; pulling
/// in the multi-line caret machinery is overkill and would break Eq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionDraft {
    /// Project id the new session belongs to. Matches the sidebar
    /// selection captured when the modal opened.
    pub project_id: String,
    /// Resolved on-disk path for the project, forwarded to the daemon
    /// as `SessionsCreateParams.project_dir`. Empty when the source
    /// snapshot does not surface a root path (Rpc source on a session
    /// without a `worktree_path` will land here); the Confirm path
    /// guards that case with a Validation toast.
    pub project_dir: String,
    /// Backend ids the user can pick. Snapshot of available backends
    /// at modal-open time so the modal's choice list stays stable even
    /// if a `daemon.health` push changes the global `App::backends`
    /// mid-modal.
    pub backends: Vec<String>,
    /// Cursor into [`backends`]. May be 0 even when `backends` is
    /// empty; Confirm guards against that.
    pub backend_idx: usize,
    /// Prompt buffer. UTF-8 char-boundary safe via the App's input
    /// handlers.
    pub prompt: String,
    /// Whether the daemon should mint a fresh git worktree on Confirm.
    pub worktree: bool,
    /// Which field has focus.
    pub field: NewSessionField,
    /// Last error surfaced inside the modal — kept on the draft so a
    /// Validation refusal stays visible while the user corrects the
    /// input, instead of disappearing on the next keystroke that
    /// happens to clear the global toast slate.
    pub error: Option<String>,
}

impl NewSessionDraft {
    /// Construct an empty draft scoped to a project.
    pub fn new(project_id: String, project_dir: String, backends: Vec<String>) -> Self {
        Self {
            project_id,
            project_dir,
            backends,
            backend_idx: 0,
            prompt: String::new(),
            worktree: false,
            field: NewSessionField::Backend,
            error: None,
        }
    }

    /// Currently selected backend, if any.
    pub fn selected_backend(&self) -> Option<&str> {
        self.backends.get(self.backend_idx).map(String::as_str)
    }

    /// Cycle to the next focusable field (Tab).
    pub fn focus_next(&mut self) {
        self.field = match self.field {
            NewSessionField::Backend => NewSessionField::Prompt,
            NewSessionField::Prompt => NewSessionField::Worktree,
            NewSessionField::Worktree => NewSessionField::Backend,
        };
    }

    /// Cycle to the previous field (Shift+Tab).
    pub fn focus_prev(&mut self) {
        self.field = match self.field {
            NewSessionField::Backend => NewSessionField::Worktree,
            NewSessionField::Prompt => NewSessionField::Backend,
            NewSessionField::Worktree => NewSessionField::Prompt,
        };
    }

    /// Move the backend cursor left.
    pub fn backend_prev(&mut self) {
        if self.backends.is_empty() {
            return;
        }
        self.backend_idx = if self.backend_idx == 0 {
            self.backends.len() - 1
        } else {
            self.backend_idx - 1
        };
    }

    /// Move the backend cursor right.
    pub fn backend_next(&mut self) {
        if self.backends.is_empty() {
            return;
        }
        self.backend_idx = (self.backend_idx + 1) % self.backends.len();
    }
}

/// Which sub-field of the [`Modal::NewSession`] form has focus. Tab /
/// Shift+Tab cycle through these in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewSessionField {
    Backend,
    Prompt,
    Worktree,
}

/// Modal overlays. At most one open at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// `d` on a session row → confirm hard-delete (PRD §5.3 二次确认).
    ConfirmDelete { session_id: String },
    /// `?` opens the full which-key overlay.
    FullHints,
    /// `n` on a project → live new-session chooser (WEK-94 / A2). The
    /// modal lets the user pick a backend, type a prompt, and toggle
    /// the worktree flag before Confirm calls
    /// [`crate::source::SessionSource::create_session`].
    NewSession(NewSessionDraft),
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
    /// `f` from the status bar's error badge: list every backend whose
    /// last probe was not `Available`, with reason + docs link. The
    /// rows are computed from the live [`crate::App::backends`]
    /// snapshot at modal-open time (WEK-36 / M3.5: the bar's "全局错误
    /// 徽标 → 错误任务列表" requirement, scoped to backends until a
    /// proper Errors tab lands in M4).
    Errors { rows: Vec<ErrorRow> },
}

/// One row of the [`Modal::Errors`] list. Mirrors the subset of
/// [`BackendBadge`] the modal renders without re-importing the badge
/// type into key_hints / runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorRow {
    pub id: String,
    pub status_label: String,
    pub reason: Option<String>,
    pub docs_url: Option<String>,
}

/// Status of the live attach pump (WEK-92-A3).
///
/// The App owns no I/O; the runner owns the actual [`crate::AttachPump`]
/// thread and translates its events into [`AppMsg::Attach*`] variants
/// that update this state. The renderer reads it each frame to decide
/// the title chrome of the transcript pane and to render the bottom
/// banner ("已断开, 重试中…").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachStatus {
    /// `sessions.attach` is in flight — the catch-up frame has not yet
    /// arrived. The transcript pane is rendered but empty.
    Connecting,
    /// The attach handshake completed. Live bytes are streaming.
    Connected { input_acquired: bool },
    /// The pump lost its connection. `will_reconnect` mirrors the
    /// pump's auto-retry budget: `true` means one attempt is queued,
    /// `false` means the user must detach + re-enter.
    Disconnected {
        reason: String,
        will_reconnect: bool,
    },
}

/// One live attach. Tracked on the App so the renderer + key router
/// can react without touching the pump thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedSession {
    pub session_id: String,
    pub project_id: String,
    pub status: AttachStatus,
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
    /// New-session modal field editing (WEK-94 / A2). Routed by the
    /// input layer only while [`Modal::NewSession`] is open.
    NewSessionFocusNext,
    NewSessionFocusPrev,
    NewSessionBackendPrev,
    NewSessionBackendNext,
    NewSessionToggleWorktree,
    /// One printable char from the user — appended to the prompt
    /// buffer when the prompt field has focus, otherwise dropped.
    NewSessionPromptChar(char),
    NewSessionPromptNewline,
    NewSessionPromptBackspace,
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
    /// Patch only the scalar counters (running / errors / cost) from a
    /// fresh `daemon.health` push. Leaves cron-derived fields alone so
    /// a health pulse doesn't blank out the "next cron" label.
    HealthUpdate(HealthSnapshot),
    /// Flip the daemon dot to "offline" without touching any other
    /// field. Sent by the IPC subscriber the moment a disconnect is
    /// detected, so the bar reflects the loss within one frame instead
    /// of waiting for the next probe interval.
    DaemonOffline,
    /// A `cron.fired` notification just landed — the bar flashes a
    /// `↻ cron-id` badge for [`Status::PULSE_TTL`].
    CronFiredEvent(CronFiredParams),
    /// `f` from a global context — open the Errors modal sourced from
    /// the current `backends` snapshot.
    OpenErrors,
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

    // --- UI prefs (WEK-42 / M4.3) -----------------------------------
    //
    // Three keystrokes mutate `[ui]` and persist back to config.toml
    // asynchronously. The App owns the in-memory copy; the runner reads
    // it each frame to pick a Palette + decide the bottom-row layout.
    //
    // We intentionally cycle through fixed enums (`Theme::next()` /
    // `KeyHintsMode::next()`) rather than offering a chooser modal:
    // M4.3 acceptance is "切换无闪屏", and a fast in-place cycle is the
    // least disruptive UX.
    /// `T` — Auto → Dark → Light → Auto.
    CycleTheme,
    /// `H` — Rich → Compact → Hidden → Rich. Distinct from the global
    /// `?` (which opens the FullHints modal); `H` reshapes the bottom
    /// row instead.
    CycleKeyHints,
    /// `C` — flip the compact layout flag. Compact mode collapses the
    /// status + hint row into one line and dims backend badges to a
    /// single colour.
    ToggleCompact,

    // --- Attach (WEK-92-A3) ----------------------------------------
    //
    // The App receives these as state updates from the runner-owned
    // attach pump. The pump itself is spawned by the runner when the
    // user presses Enter on a [`Selection::Session`] row; the App
    // simply records intent and reacts to the pump's status changes.
    /// User asked to attach to the selected session. The App flips
    /// focus to `Transcript` and records the session id; the runner
    /// observes [`App::attached`] becoming `Some(Connecting)` and
    /// spawns the pump.
    BeginAttach {
        session_id: String,
        project_id: String,
    },
    /// Pump finished its attach handshake.
    AttachConnected {
        session_id: String,
        snapshot_seq: u64,
        input_acquired: bool,
    },
    /// Pump lost its connection.
    AttachDisconnected {
        session_id: String,
        reason: String,
        will_reconnect: bool,
    },
    /// Pump exited permanently (user detach or second reconnect
    /// failure). The App clears `attached` and returns focus to the
    /// sidebar.
    AttachClosed,
    /// User asked to detach (Ctrl+B then `d`, see runner). Clears the
    /// attach state on the App; the runner emits the actual `detach`
    /// command on the pump.
    Detach,
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
    /// Live `[ui]` preferences (WEK-42 / M4.3). Mutated in place by the
    /// `CycleTheme` / `CycleKeyHints` / `ToggleCompact` handlers; the
    /// runner reads it each frame to build the [`crate::theme::Palette`]
    /// and decide the bottom-row layout.
    pub ui_prefs: UiPrefs,
    /// Where to persist `ui_prefs` on every mutation. `None` disables
    /// persistence (default for in-process tests; production wires
    /// [`crate::ui_prefs::default_config_path`]).
    pub ui_prefs_path: Option<PathBuf>,
    /// Live attach state, if any (WEK-92-A3). `Some` while a session
    /// pane is open; cleared on detach / close. The runner reads this
    /// each frame to decide whether to render the transcript pane and
    /// where to forward keystrokes.
    pub attached: Option<AttachedSession>,
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
            status: Status::default(),
            backends: Vec::new(),
            last_toast: None,
            ui_prefs: UiPrefs::default(),
            ui_prefs_path: None,
            attached: None,
        };
        app.refresh_sessions();
        app
    }

    /// Inject the persisted UI prefs and (optionally) the path to write
    /// future mutations to. The binary calls this once at startup after
    /// resolving [`crate::ui_prefs::default_config_path`]; tests can
    /// leave both parameters as defaults and skip persistence entirely.
    pub fn with_ui_prefs(mut self, prefs: UiPrefs, path: Option<PathBuf>) -> Self {
        self.ui_prefs = prefs;
        self.ui_prefs_path = path;
        self
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
        // Background system notifications must update the status bar even
        // when a modal is open, otherwise daemon health / cron pulses /
        // disconnect events arriving during a modal session are dropped
        // forever (review fix for WEK-36: the modal short-circuit was
        // silently swallowing them, breaking the "<1s latency" and
        // "auto-recover" acceptance).
        let msg = match self.try_apply_system_notification(msg) {
            None => return AppOutcome::Continue,
            Some(other) => other,
        };
        // WEK-42 / M4.3 review fix: T/H/C are advertised as globals and
        // the translator routes them inside modals, but the modal
        // short-circuit below would otherwise drop them on the floor.
        // Apply UI-pref toggles BEFORE the modal short-circuit so the
        // user can read the binding in `?` overlay and immediately try
        // it without closing the overlay first. The modal stays open —
        // the toggle is a global preference change, not a modal action.
        if let Some(other) = self.try_apply_ui_pref(msg) {
            // try_apply_ui_pref returned the message back ⇒ not a pref.
            // Fall through to the normal modal/keybinding path.
            let msg = other;
            // WEK-94: NewSession modal field edits mutate the draft in
            // place, never close the modal. Route them BEFORE the modal
            // short-circuit so they don't have to share the
            // confirm/cancel matrix.
            let msg = match self.try_apply_new_session_field(msg) {
                None => return AppOutcome::Continue,
                Some(other) => other,
            };
            // Modal short-circuits — while a modal is open, only its keys are
            // valid (PRD §5.6 上下文驱动).
            if let Some(modal) = self.modal.clone() {
                return self.handle_in_modal(modal, msg);
            }
            return self.dispatch_non_modal(msg);
        }
        AppOutcome::Continue
    }

    /// Handle one user-input message in the non-modal context. Split out
    /// of [`Self::handle`] so the pre-modal UI-pref short-circuit can
    /// fall through cleanly without an extra level of nesting.
    fn dispatch_non_modal(&mut self, msg: AppMsg) -> AppOutcome {
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
            AppMsg::OpenErrors => {
                let rows = self
                    .backends
                    .iter()
                    .filter(|b| b.is_unavailable())
                    .map(|b| ErrorRow {
                        id: b.id.clone(),
                        status_label: b.status_label().to_string(),
                        reason: b.reason.clone(),
                        docs_url: b.docs_url.clone(),
                    })
                    .collect();
                self.modal = Some(Modal::Errors { rows });
            }
            AppMsg::RefreshSessions => self.refresh_sessions(),
            // System notifications (StatusUpdate / HealthUpdate /
            // DaemonOffline / CronFiredEvent / BackendsUpdate) are
            // handled before the modal short-circuit by
            // `try_apply_system_notification`; reaching them here is
            // unreachable but we keep no-op arms instead of a panic so a
            // future refactor that re-enters this match by a different
            // path doesn't crash the TUI.
            AppMsg::StatusUpdate(_)
            | AppMsg::HealthUpdate(_)
            | AppMsg::DaemonOffline
            | AppMsg::CronFiredEvent(_)
            | AppMsg::BackendsUpdate(_) => {}

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

            // --- UI prefs ----------------------------------------------
            //
            // These three variants are normally applied BEFORE the modal
            // short-circuit by [`Self::try_apply_ui_pref`] so that `T`/
            // `H`/`C` work as true globals even inside a modal (review
            // fix for WEK-42 / M4.3). Reaching this arm is the
            // non-modal fallthrough; the toggle + persist + toast logic
            // is shared with the pre-modal path, so we just delegate.
            AppMsg::CycleTheme | AppMsg::CycleKeyHints | AppMsg::ToggleCompact => {
                // try_apply_ui_pref consumes the message (returns None)
                // when it matches one of the three UI-pref variants.
                let _ = self.try_apply_ui_pref(msg);
            }

            // --- Attach dispatch (WEK-92-A3) -------------------------
            AppMsg::BeginAttach {
                session_id,
                project_id,
            } => {
                if self
                    .attached
                    .as_ref()
                    .is_some_and(|a| a.session_id == session_id)
                {
                    return AppOutcome::Continue;
                }
                self.attached = Some(AttachedSession {
                    session_id,
                    project_id,
                    status: AttachStatus::Connecting,
                });
                self.focus = Focus::Transcript;
            }
            AppMsg::AttachConnected {
                session_id,
                snapshot_seq: _,
                input_acquired,
            } => {
                if let Some(att) = self.attached.as_mut() {
                    if att.session_id == session_id {
                        att.status = AttachStatus::Connected { input_acquired };
                    }
                }
            }
            AppMsg::AttachDisconnected {
                session_id,
                reason,
                will_reconnect,
            } => {
                if let Some(att) = self.attached.as_mut() {
                    if att.session_id == session_id {
                        att.status = AttachStatus::Disconnected {
                            reason: reason.clone(),
                            will_reconnect,
                        };
                        self.last_toast = Some(if will_reconnect {
                            format!("attach: {reason} — 重试中…")
                        } else {
                            format!("attach: {reason}")
                        });
                    }
                }
            }
            AppMsg::AttachClosed => {
                self.attached = None;
                self.focus = Focus::Sidebar;
            }
            AppMsg::Detach => {
                // The App clears its state immediately so the renderer
                // returns to the sidebar on the next frame; the runner
                // tears down the pump asynchronously after observing
                // `attached == None`.
                if self.attached.is_some() {
                    self.attached = None;
                    self.focus = Focus::Sidebar;
                    self.last_toast = Some("detached".into());
                }
            }

            // --- NewSession modal field edits (WEK-94 / A2) ----------
            //
            // These variants are consumed by
            // [`Self::try_apply_new_session_field`] BEFORE the modal
            // short-circuit. Reaching here means no NewSession modal
            // was open (the user mashed Tab while the sidebar had
            // focus); silently drop so the key is a no-op rather than
            // a panic.
            AppMsg::NewSessionFocusNext
            | AppMsg::NewSessionFocusPrev
            | AppMsg::NewSessionBackendPrev
            | AppMsg::NewSessionBackendNext
            | AppMsg::NewSessionToggleWorktree
            | AppMsg::NewSessionPromptChar(_)
            | AppMsg::NewSessionPromptNewline
            | AppMsg::NewSessionPromptBackspace => {}
        }
        AppOutcome::Continue
    }

    /// Write the current `ui_prefs` to `ui_prefs_path` if one is set.
    /// Failures are downgraded to a toast (the in-memory pref still
    /// applies for the rest of the session) so a read-only config dir or
    /// a malformed sibling section doesn't kill the user's keystroke.
    fn persist_ui_prefs(&mut self) {
        let Some(path) = self.ui_prefs_path.as_ref() else {
            return;
        };
        if let Err(e) = crate::ui_prefs::save(path, &self.ui_prefs) {
            self.last_toast = Some(format!("config save failed: {e}"));
        }
    }

    /// Apply a UI-pref toggle (T/H/C) before the modal short-circuit.
    /// Returns `None` when the message was a pref and has been applied
    /// (the modal — if any — stays open and we end this `handle` tick);
    /// returns `Some(msg)` otherwise so the caller can fall through to
    /// the modal/non-modal dispatch.
    ///
    /// The user-facing intent (WEK-42 / M4.3 review): when `?` overlay
    /// is open and the user presses `T`, the theme should change AND
    /// the overlay should stay visible so they can keep reading. The
    /// modal short-circuit downstream would otherwise route into
    /// [`Self::handle_in_modal`], which has no arm for these messages
    /// and silently drops them.
    fn try_apply_ui_pref(&mut self, msg: AppMsg) -> Option<AppMsg> {
        match msg {
            AppMsg::CycleTheme => {
                self.ui_prefs.theme = self.ui_prefs.theme.next();
                self.persist_ui_prefs();
                self.last_toast = Some(format!("theme: {}", self.ui_prefs.theme.label()));
                None
            }
            AppMsg::CycleKeyHints => {
                self.ui_prefs.key_hints = self.ui_prefs.key_hints.next();
                self.persist_ui_prefs();
                self.last_toast = Some(format!("key hints: {}", self.ui_prefs.key_hints.label()));
                None
            }
            AppMsg::ToggleCompact => {
                self.ui_prefs.compact = !self.ui_prefs.compact;
                self.persist_ui_prefs();
                self.last_toast = Some(format!(
                    "compact: {}",
                    if self.ui_prefs.compact { "on" } else { "off" }
                ));
                None
            }
            other => Some(other),
        }
    }

    /// Apply a [`Modal::NewSession`] field-edit message in place. Run
    /// before the modal short-circuit so the per-modal `match` doesn't
    /// have to enumerate every field-edit variant; returns `None` when
    /// the message was consumed (modal stays open), otherwise hands the
    /// message back.
    ///
    /// All branches no-op when [`Modal::NewSession`] is not the open
    /// modal — that keeps the helper trivially safe to call before the
    /// general modal dispatch.
    fn try_apply_new_session_field(&mut self, msg: AppMsg) -> Option<AppMsg> {
        let draft = match &mut self.modal {
            Some(Modal::NewSession(d)) => d,
            _ => return Some(msg),
        };
        // Any field edit clears the previous error so a stale "prompt
        // cannot be empty" doesn't linger over a freshly-typed
        // character.
        let clear_err_on = |d: &mut NewSessionDraft| {
            d.error = None;
        };
        match msg {
            AppMsg::NewSessionFocusNext => {
                draft.focus_next();
                clear_err_on(draft);
            }
            AppMsg::NewSessionFocusPrev => {
                draft.focus_prev();
                clear_err_on(draft);
            }
            AppMsg::NewSessionBackendPrev => {
                if draft.field == NewSessionField::Backend {
                    draft.backend_prev();
                    clear_err_on(draft);
                } else {
                    return Some(msg);
                }
            }
            AppMsg::NewSessionBackendNext => {
                if draft.field == NewSessionField::Backend {
                    draft.backend_next();
                    clear_err_on(draft);
                } else {
                    return Some(msg);
                }
            }
            AppMsg::NewSessionToggleWorktree => {
                if draft.field == NewSessionField::Worktree {
                    draft.worktree = !draft.worktree;
                    clear_err_on(draft);
                } else {
                    return Some(msg);
                }
            }
            AppMsg::NewSessionPromptChar(c) => {
                if draft.field == NewSessionField::Prompt {
                    draft.prompt.push(c);
                    clear_err_on(draft);
                } else {
                    // Char hit but prompt isn't focused — swallow so
                    // it doesn't drop through to the modal's
                    // confirm/cancel matrix.
                }
            }
            AppMsg::NewSessionPromptNewline => {
                if draft.field == NewSessionField::Prompt {
                    draft.prompt.push('\n');
                    clear_err_on(draft);
                }
            }
            AppMsg::NewSessionPromptBackspace => {
                if draft.field == NewSessionField::Prompt {
                    draft.prompt.pop();
                    clear_err_on(draft);
                }
            }
            other => return Some(other),
        }
        None
    }

    /// Stamp an error onto the open [`Modal::NewSession`] draft. Used
    /// by the Confirm path so a Validation refusal surfaces inside the
    /// modal AND (via the caller) on the status toast.
    fn update_new_session_error(&mut self, why: String) {
        if let Some(Modal::NewSession(ref mut d)) = self.modal {
            d.error = Some(why);
        }
    }

    /// Apply a background system notification — daemon health, cron
    /// pulse, disconnect, or a fresh status snapshot. These messages are
    /// **not** user input; they originate from the IPC pump and must
    /// land on the status bar regardless of whether a modal is open.
    ///
    /// Returns `None` when the message was a system notification and has
    /// been applied; returns `Some(msg)` (handing the value back) when
    /// the message is a user-input variant that the caller should route
    /// through the normal modal/keybinding dispatch.
    fn try_apply_system_notification(&mut self, msg: AppMsg) -> Option<AppMsg> {
        match msg {
            AppMsg::StatusUpdate(s) => self.status = s,
            AppMsg::HealthUpdate(h) => {
                self.status.daemon_online = true;
                self.status.running = h.running as usize;
                self.status.errors_last_5m = h.errors_last_5m;
            }
            AppMsg::DaemonOffline => {
                self.status.daemon_online = false;
            }
            AppMsg::CronFiredEvent(p) => {
                let fired_at = chrono::DateTime::parse_from_rfc3339(&p.fired_at)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                self.status.last_cron_pulse = Some(CronPulse {
                    cron_id: p.cron_id,
                    fired_at,
                });
            }
            AppMsg::BackendsUpdate(b) => {
                // Sort by id so the rendered order is stable across
                // pulses (the daemon already sorts, but a paranoid TUI
                // can't hurt).
                let mut sorted = b;
                sorted.sort_by(|a, b| a.id.cmp(&b.id));
                self.backends = sorted;
            }
            other => return Some(other),
        }
        None
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
            (Modal::NewSession(draft), AppMsg::Confirm) => {
                // Validate first so the user sees a precise reason and
                // the modal can stay open for them to fix it (per
                // architect review: no backends / empty prompt should
                // surface a toast, not silently no-op).
                let trimmed_prompt = draft.prompt.trim().to_string();
                let backend = match draft.selected_backend() {
                    Some(b) => b.to_string(),
                    None => {
                        let msg = "no available backend, start a backend first".to_string();
                        self.update_new_session_error(msg.clone());
                        self.last_toast = Some(format!("new session: {msg}"));
                        return AppOutcome::Continue;
                    }
                };
                if trimmed_prompt.is_empty() {
                    let msg = "prompt cannot be empty".to_string();
                    self.update_new_session_error(msg.clone());
                    self.last_toast = Some(format!("new session: {msg}"));
                    return AppOutcome::Continue;
                }
                if draft.project_dir.trim().is_empty() {
                    let msg = "project directory missing for this row".to_string();
                    self.update_new_session_error(msg.clone());
                    self.last_toast = Some(format!("new session: {msg}"));
                    return AppOutcome::Continue;
                }
                let req = NewSessionRequest {
                    project_dir: draft.project_dir.clone(),
                    backend,
                    args: Vec::new(),
                    prompt: trimmed_prompt,
                    worktree: draft.worktree,
                };
                match self.source.create_session(req) {
                    Ok(id) => {
                        self.modal = None;
                        self.refresh_sessions();
                        self.last_toast = Some(format!("created session {id}"));
                    }
                    Err(SourceError::Validation(why)) => {
                        // Keep the modal open so the user can adjust
                        // the input that the daemon also rejected.
                        self.update_new_session_error(why.clone());
                        self.last_toast = Some(format!("new session: {why}"));
                    }
                    Err(SourceError::Backend(why)) => {
                        // Close the modal — the user cannot fix a
                        // daemon-side refusal from this form. Surface
                        // the reason via the status toast instead.
                        self.modal = None;
                        self.last_toast = Some(format!("new session failed: {why}"));
                    }
                }
            }
            (Modal::NewSession(_), AppMsg::Cancel) => {
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
            (Modal::Errors { .. }, AppMsg::Cancel) | (Modal::Errors { .. }, AppMsg::Confirm) => {
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
        match self.sidebar.selection() {
            Selection::Group { .. } => {
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
            Selection::Session {
                project_id,
                session_id,
            } => {
                // WEK-92-A3: attach to the session. Record the intent on
                // the App; the runner observes `attached = Some(Connecting)`
                // and spawns the pump. We refuse re-entry into the same
                // session (the pump would mis-acquire input).
                if self
                    .attached
                    .as_ref()
                    .is_some_and(|a| a.session_id == session_id)
                {
                    return;
                }
                self.attached = Some(AttachedSession {
                    session_id,
                    project_id,
                    status: AttachStatus::Connecting,
                });
                self.focus = Focus::Transcript;
            }
            Selection::Empty => {}
        }
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
        // Resolve the on-disk root for the project so the daemon
        // receives a real `project_dir`. Sidebar groups keep it in
        // `root_path`; when empty (the Rpc source can land here when
        // the underlying session has no `worktree_path` — that's the
        // common shape for non-worktree sessions, see
        // la-core/src/manager.rs) we MUST NOT fall back to the
        // project_id because the daemon's `sessions.create` interprets
        // `project_dir` as a filesystem path and would either create a
        // bogus project under that UUID string or refuse the spawn
        // outright (review fix on PR #86: WEK-94 blocker). Keep the
        // value empty so Confirm hits the `project_dir is missing`
        // Validation branch and the user sees a precise error.
        let project_dir = self
            .sidebar
            .groups()
            .iter()
            .find(|g| g.project_id == project_id)
            .map(|g| g.root_path.clone())
            .unwrap_or_default();
        // Snapshot available backends from the live health pulse.
        // Unavailable ones (NotInstalled / Unauthenticated / etc.) are
        // filtered out so the user cannot pick a backend the daemon
        // will immediately refuse — but we still open the modal even
        // when the list ends up empty so the key path stays visible;
        // Confirm surfaces the empty case as a Validation toast (per
        // architect review).
        let backends: Vec<String> = self
            .backends
            .iter()
            .filter(|b| !b.is_unavailable())
            .map(|b| b.id.clone())
            .collect();
        self.modal = Some(Modal::NewSession(NewSessionDraft::new(
            project_id,
            project_dir,
            backends,
        )));
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

    /// Stage a NewSession modal with one available backend so the tests
    /// below can drive Confirm without rehearsing the BackendsUpdate +
    /// SidebarDown choreography in every body.
    fn open_new_session_modal(a: &mut App<MockSessionSource>) {
        use la_proto::notifications::BackendHealthStatus;
        a.handle(AppMsg::BackendsUpdate(vec![BackendBadge {
            id: "claude".into(),
            display_name: "Claude".into(),
            status: BackendHealthStatus::Available,
            reason: None,
            docs_url: None,
            version: Some("2.1".into()),
        }]));
        // First sidebar item is the p-a group header.
        a.handle(AppMsg::NewSession);
    }

    #[test]
    fn n_opens_new_session_modal_seeded_with_available_backends_and_project_dir() {
        let mut a = app();
        open_new_session_modal(&mut a);
        let Some(Modal::NewSession(ref draft)) = a.modal else {
            panic!("expected NewSession modal, got {:?}", a.modal);
        };
        assert_eq!(draft.project_id, "p-a");
        assert_eq!(draft.project_dir, "~/code/proj-a");
        assert_eq!(draft.backends, vec!["claude".to_string()]);
        assert_eq!(draft.backend_idx, 0);
        assert!(draft.prompt.is_empty());
        assert!(!draft.worktree);
        assert_eq!(draft.field, NewSessionField::Backend);
        assert!(draft.error.is_none());
    }

    #[test]
    fn confirm_with_backend_and_prompt_calls_source_with_correct_request() {
        // WEK-94 / A2: UI ↔ trait edge — the modal hands the source
        // the exact (project_dir, backend, prompt, worktree) it
        // collected, and a successful return closes the modal and
        // refreshes the sidebar.
        let mut a = app();
        open_new_session_modal(&mut a);
        a.handle(AppMsg::NewSessionFocusNext); // Backend -> Prompt
        for c in "fix the login bug".chars() {
            a.handle(AppMsg::NewSessionPromptChar(c));
        }
        a.handle(AppMsg::NewSessionFocusNext); // Prompt -> Worktree
        a.handle(AppMsg::NewSessionToggleWorktree);
        a.handle(AppMsg::Confirm);
        assert!(a.modal.is_none(), "modal closed on successful create");
        let created = a.source().created();
        assert_eq!(created.len(), 1, "exactly one create call");
        assert_eq!(created[0].project_dir, "~/code/proj-a");
        assert_eq!(created[0].backend, "claude");
        assert_eq!(created[0].prompt, "fix the login bug");
        assert!(created[0].worktree);
        assert_eq!(created[0].args, Vec::<String>::new());
        // The mock inserted a `Running` row under proj-a — refresh
        // happened so the sidebar can find it.
        let snap = a.source().snapshot();
        let proj_a = snap.iter().find(|g| g.project_id == "p-a").unwrap();
        let new_row = proj_a
            .sessions
            .iter()
            .find(|s| s.session_id.starts_with("mock-"))
            .expect("new mock row landed under proj-a");
        assert_eq!(new_row.backend.id(), "claude");
    }

    #[test]
    fn empty_prompt_blocks_confirm_with_toast_and_keeps_modal_open() {
        let mut a = app();
        open_new_session_modal(&mut a);
        // Backend focused, prompt empty.
        a.handle(AppMsg::Confirm);
        // Modal still open with an error stamped on the draft.
        let Some(Modal::NewSession(ref draft)) = a.modal else {
            panic!("expected NewSession modal still open, got {:?}", a.modal);
        };
        assert_eq!(draft.error.as_deref(), Some("prompt cannot be empty"));
        assert!(a
            .last_toast
            .as_deref()
            .map(|t| t.contains("prompt cannot be empty"))
            .unwrap_or(false));
        assert!(
            a.source().created().is_empty(),
            "source.create_session was not called"
        );
    }

    #[test]
    fn no_backends_blocks_confirm_with_toast() {
        // WEK-94 / A2: even when the modal opens with an empty backend
        // list (no available backends in the BackendsUpdate snapshot),
        // Confirm must surface a Validation toast rather than silently
        // no-op.
        let mut a = app();
        a.handle(AppMsg::NewSession);
        let Some(Modal::NewSession(ref draft)) = a.modal else {
            panic!("expected modal, got {:?}", a.modal);
        };
        assert!(draft.backends.is_empty(), "no backends fed in yet");
        a.handle(AppMsg::Confirm);
        let Some(Modal::NewSession(ref draft)) = a.modal else {
            panic!("modal closed unexpectedly");
        };
        assert!(draft
            .error
            .as_deref()
            .map(|e| e.contains("no available backend"))
            .unwrap_or(false));
        assert!(a
            .last_toast
            .as_deref()
            .map(|t| t.contains("no available backend"))
            .unwrap_or(false));
        assert!(a.source().created().is_empty());
    }

    #[test]
    fn empty_root_path_blocks_confirm_and_does_not_call_create() {
        // WEK-94 / PR #86 review (Code Reviewer blocker): the Rpc source
        // can land sessions whose `worktree_path` is None, which
        // surfaces in the sidebar as a `ProjectGroup` with an empty
        // `root_path`. The earlier draft fell back to `project_id`
        // (commonly a UUID under the Rpc source) as `project_dir`,
        // which the daemon would either turn into a bogus project under
        // that UUID string or fail to spawn entirely. The fix is to
        // keep `project_dir` empty so the Confirm path hits the
        // `project directory missing` Validation arm before any RPC.
        //
        // Build a source whose project has a registered id but an
        // empty `root_path` so the live Rpc shape is faithfully
        // reproduced — the App treats both sources identically.
        let mut src = MockSessionSource::new();
        src.add_project("proj-a-uuid", "proj-a", "");
        src.add_session(
            "s1",
            "proj-a-uuid",
            "claude",
            None,
            crate::model::RunState::Idle,
        );
        let mut a = App::new(src);
        use la_proto::notifications::BackendHealthStatus;
        a.handle(AppMsg::BackendsUpdate(vec![BackendBadge {
            id: "claude".into(),
            display_name: "Claude".into(),
            status: BackendHealthStatus::Available,
            reason: None,
            docs_url: None,
            version: Some("2.1".into()),
        }]));
        a.handle(AppMsg::NewSession);
        let Some(Modal::NewSession(ref draft)) = a.modal else {
            panic!("expected NewSession modal, got {:?}", a.modal);
        };
        assert_eq!(
            draft.project_dir, "",
            "empty root_path must NOT fall back to project_id — that would smuggle a UUID into `sessions.create.project_dir`"
        );
        // Type a real prompt so the only blocker left is project_dir.
        a.handle(AppMsg::NewSessionFocusNext);
        for c in "go".chars() {
            a.handle(AppMsg::NewSessionPromptChar(c));
        }
        a.handle(AppMsg::Confirm);
        let Some(Modal::NewSession(ref draft)) = a.modal else {
            panic!("modal must stay open on Validation refusal");
        };
        assert!(
            draft
                .error
                .as_deref()
                .map(|e| e.contains("project directory"))
                .unwrap_or(false),
            "expected project_directory Validation error, got {:?}",
            draft.error
        );
        assert!(
            a.source().created().is_empty(),
            "create_session must NOT be called when project_dir is empty"
        );
    }

    #[test]
    fn modal_field_keys_route_to_their_target_field() {
        // WEK-94 / A2: Tab cycles Backend → Prompt → Worktree → Backend;
        // ←/→ only move the backend cursor while Backend is focused;
        // Space only toggles worktree while Worktree is focused; printable
        // chars only land in the prompt buffer while Prompt is focused.
        // This pins the cross-field invariants in one place so a future
        // refactor of the routing helpers can't silently break them.
        let mut a = app();
        open_new_session_modal(&mut a);
        // Stage two backends so backend cursor movement is observable.
        if let Some(Modal::NewSession(ref mut d)) = a.modal {
            d.backends = vec!["claude".into(), "codex".into()];
        }
        // Backend → ←/→ cycles index but never reaches outside [0, len).
        a.handle(AppMsg::NewSessionBackendNext);
        a.handle(AppMsg::NewSessionBackendNext);
        if let Some(Modal::NewSession(ref d)) = a.modal {
            assert_eq!(d.backend_idx, 0, "cycle wraps");
        }
        // Char on Backend → swallowed (does not land in prompt).
        a.handle(AppMsg::NewSessionPromptChar('z'));
        if let Some(Modal::NewSession(ref d)) = a.modal {
            assert!(
                d.prompt.is_empty(),
                "prompt char dropped while Backend focused"
            );
        }
        // Focus → Prompt; chars now land.
        a.handle(AppMsg::NewSessionFocusNext);
        a.handle(AppMsg::NewSessionPromptChar('h'));
        a.handle(AppMsg::NewSessionPromptChar('i'));
        a.handle(AppMsg::NewSessionPromptNewline);
        a.handle(AppMsg::NewSessionPromptChar('!'));
        a.handle(AppMsg::NewSessionPromptBackspace);
        if let Some(Modal::NewSession(ref d)) = a.modal {
            assert_eq!(d.prompt, "hi\n");
        }
        // Space on Prompt → printable char (handled by input layer); the
        // App's `NewSessionToggleWorktree` arm refuses to toggle while
        // Prompt is focused.
        a.handle(AppMsg::NewSessionToggleWorktree);
        if let Some(Modal::NewSession(ref d)) = a.modal {
            assert!(!d.worktree, "worktree untoggled outside its field");
        }
        // Focus → Worktree; Space toggles.
        a.handle(AppMsg::NewSessionFocusNext);
        a.handle(AppMsg::NewSessionToggleWorktree);
        if let Some(Modal::NewSession(ref d)) = a.modal {
            assert!(d.worktree);
        }
        // Esc closes.
        a.handle(AppMsg::Cancel);
        assert!(a.modal.is_none());
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
            ..Status::default()
        };
        a.handle(AppMsg::StatusUpdate(s.clone()));
        assert_eq!(a.status.running, 3);
        assert!(a.status.daemon_online);
        assert_eq!(a.status.next_cron_label.as_deref(), Some("next 02:00"));
    }

    #[test]
    fn health_update_patches_counters_without_clobbering_cron_label() {
        let mut a = app();
        a.handle(AppMsg::StatusUpdate(Status {
            daemon_online: false,
            running: 0,
            next_cron_label: Some("next 02:00".into()),
            right_context: "main".into(),
            ..Status::default()
        }));
        a.handle(AppMsg::HealthUpdate(HealthSnapshot {
            running: 2,
            queue_depth: 0,
            errors_last_5m: 1,
        }));
        assert!(a.status.daemon_online, "health update flips daemon online");
        assert_eq!(a.status.running, 2);
        assert_eq!(a.status.errors_last_5m, 1);
        // Cron label is untouched.
        assert_eq!(a.status.next_cron_label.as_deref(), Some("next 02:00"));
        // Right context is also untouched.
        assert_eq!(a.status.right_context, "main");
    }

    #[test]
    fn daemon_offline_flips_only_dot() {
        let mut a = app();
        a.handle(AppMsg::HealthUpdate(HealthSnapshot {
            running: 4,
            queue_depth: 0,
            errors_last_5m: 0,
        }));
        assert!(a.status.daemon_online);
        a.handle(AppMsg::DaemonOffline);
        assert!(!a.status.daemon_online);
        // Counters keep their last-known value so a flicker offline → online
        // doesn't blank the bar to "0 running" for a frame.
        assert_eq!(a.status.running, 4);
    }

    #[test]
    fn cron_fired_event_sets_last_pulse() {
        let mut a = app();
        a.handle(AppMsg::CronFiredEvent(CronFiredParams {
            cron_id: "nightly".into(),
            run_id: "r1".into(),
            fired_at: "2026-06-03T00:00:00Z".into(),
            status: "spawning".into(),
        }));
        let pulse = a.status.last_cron_pulse.as_ref().expect("pulse");
        assert_eq!(pulse.cron_id, "nightly");
        assert_eq!(pulse.fired_at.to_rfc3339(), "2026-06-03T00:00:00+00:00");
    }

    #[test]
    fn open_errors_modal_lists_unavailable_backends_only() {
        let mut a = app();
        use la_proto::notifications::BackendHealthStatus;
        a.handle(AppMsg::BackendsUpdate(vec![
            BackendBadge {
                id: "claude".into(),
                display_name: "Claude".into(),
                status: BackendHealthStatus::Available,
                reason: None,
                docs_url: None,
                version: Some("2.1".into()),
            },
            BackendBadge {
                id: "codex".into(),
                display_name: "Codex".into(),
                status: BackendHealthStatus::NotInstalled,
                reason: Some("not on PATH".into()),
                docs_url: Some("https://example.com/install".into()),
                version: None,
            },
        ]));
        a.handle(AppMsg::OpenErrors);
        let Some(Modal::Errors { rows }) = &a.modal else {
            panic!("expected Errors modal, got {:?}", a.modal);
        };
        assert_eq!(rows.len(), 1, "available backend filtered out");
        assert_eq!(rows[0].id, "codex");
        assert_eq!(rows[0].reason.as_deref(), Some("not on PATH"));
        // Closing via Esc/Cancel clears it.
        a.handle(AppMsg::Cancel);
        assert!(a.modal.is_none());
    }

    #[test]
    fn system_notifications_apply_while_modal_is_open() {
        // Review fix for WEK-36: any modal short-circuit in `handle`
        // must not swallow daemon health / cron / disconnect events.
        // Open a modal, then push the full set of system notifications
        // and assert each one landed on the status bar (modal still open,
        // bar still updated).
        use la_proto::notifications::BackendHealthStatus;

        let mut a = app();
        // Open a modal that has nothing to do with health (FullHints is
        // the cheapest one — no setup, no source mutation).
        a.handle(AppMsg::ToggleFullHints);
        assert!(matches!(a.modal, Some(Modal::FullHints)));

        a.handle(AppMsg::HealthUpdate(HealthSnapshot {
            running: 3,
            queue_depth: 0,
            errors_last_5m: 2,
        }));
        assert!(matches!(a.modal, Some(Modal::FullHints)), "modal stays");
        assert!(a.status.daemon_online);
        assert_eq!(a.status.running, 3);
        assert_eq!(a.status.errors_last_5m, 2);

        a.handle(AppMsg::BackendsUpdate(vec![BackendBadge {
            id: "claude".into(),
            display_name: "Claude".into(),
            status: BackendHealthStatus::NotInstalled,
            reason: Some("missing".into()),
            docs_url: None,
            version: None,
        }]));
        assert!(matches!(a.modal, Some(Modal::FullHints)));
        assert_eq!(a.backends.len(), 1);
        assert_eq!(a.backends[0].id, "claude");

        a.handle(AppMsg::CronFiredEvent(CronFiredParams {
            cron_id: "nightly".into(),
            run_id: "r1".into(),
            fired_at: "2026-06-03T01:00:00Z".into(),
            status: "spawning".into(),
        }));
        assert!(matches!(a.modal, Some(Modal::FullHints)));
        let pulse = a.status.last_cron_pulse.as_ref().expect("pulse landed");
        assert_eq!(pulse.cron_id, "nightly");

        a.handle(AppMsg::DaemonOffline);
        assert!(matches!(a.modal, Some(Modal::FullHints)));
        assert!(!a.status.daemon_online);

        a.handle(AppMsg::StatusUpdate(Status {
            daemon_online: true,
            running: 7,
            right_context: "branch-x".into(),
            ..Status::default()
        }));
        assert!(matches!(a.modal, Some(Modal::FullHints)));
        assert_eq!(a.status.running, 7);
        assert_eq!(a.status.right_context, "branch-x");
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

    // -----------------------------------------------------------------
    // WEK-92-A3: attach state machine on the App
    // -----------------------------------------------------------------

    fn navigate_to_first_session(app: &mut App<MockSessionSource>) -> String {
        loop {
            app.handle(AppMsg::SidebarDown);
            if let Selection::Session { session_id, .. } = app.sidebar.selection() {
                return session_id;
            }
            if matches!(app.sidebar.selection(), Selection::Empty) {
                panic!("no session in fixture");
            }
        }
    }

    #[test]
    fn enter_on_session_row_starts_attach_and_flips_focus() {
        let mut a = app();
        let sid = navigate_to_first_session(&mut a);
        a.handle(AppMsg::Enter);
        let att = a.attached.as_ref().expect("attach state present");
        assert_eq!(att.session_id, sid);
        assert_eq!(att.status, AttachStatus::Connecting);
        assert_eq!(a.focus, Focus::Transcript);
    }

    #[test]
    fn enter_twice_on_same_session_is_idempotent() {
        let mut a = app();
        let _ = navigate_to_first_session(&mut a);
        a.handle(AppMsg::Enter);
        let snap1 = a.attached.clone();
        a.handle(AppMsg::Enter);
        assert_eq!(
            a.attached, snap1,
            "re-entering same session must not reset state"
        );
    }

    #[test]
    fn attach_connected_updates_status_but_keeps_focus() {
        let mut a = app();
        let sid = navigate_to_first_session(&mut a);
        a.handle(AppMsg::Enter);
        a.handle(AppMsg::AttachConnected {
            session_id: sid.clone(),
            snapshot_seq: 42,
            input_acquired: true,
        });
        let att = a.attached.as_ref().expect("still attached");
        assert_eq!(
            att.status,
            AttachStatus::Connected {
                input_acquired: true
            }
        );
        assert_eq!(a.focus, Focus::Transcript);
    }

    #[test]
    fn attach_disconnected_flips_status_and_surfaces_toast() {
        let mut a = app();
        let sid = navigate_to_first_session(&mut a);
        a.handle(AppMsg::Enter);
        a.handle(AppMsg::AttachDisconnected {
            session_id: sid,
            reason: "daemon gone".into(),
            will_reconnect: true,
        });
        let att = a.attached.as_ref().expect("still attached");
        assert!(matches!(
            att.status,
            AttachStatus::Disconnected {
                will_reconnect: true,
                ..
            }
        ));
        assert!(
            a.last_toast
                .as_deref()
                .is_some_and(|t| t.contains("重试中")),
            "expected retry toast, got {:?}",
            a.last_toast
        );
    }

    #[test]
    fn attach_closed_clears_state_and_returns_focus_to_sidebar() {
        let mut a = app();
        let _ = navigate_to_first_session(&mut a);
        a.handle(AppMsg::Enter);
        a.handle(AppMsg::AttachClosed);
        assert!(a.attached.is_none());
        assert_eq!(a.focus, Focus::Sidebar);
    }

    #[test]
    fn detach_msg_drops_state_immediately() {
        let mut a = app();
        let _ = navigate_to_first_session(&mut a);
        a.handle(AppMsg::Enter);
        a.handle(AppMsg::Detach);
        assert!(a.attached.is_none());
        assert_eq!(a.focus, Focus::Sidebar);
        assert_eq!(a.last_toast.as_deref(), Some("detached"));
    }

    #[test]
    fn attach_messages_for_unrelated_session_are_ignored() {
        let mut a = app();
        let _ = navigate_to_first_session(&mut a);
        a.handle(AppMsg::Enter);
        let before = a.attached.clone();
        a.handle(AppMsg::AttachConnected {
            session_id: "some-other-session".into(),
            snapshot_seq: 0,
            input_acquired: true,
        });
        assert_eq!(
            a.attached, before,
            "stray pump events must not mutate state"
        );
    }
}
