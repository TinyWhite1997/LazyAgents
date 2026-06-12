//! Crossterm `Event` → [`crate::app::AppMsg`] translator.
//!
//! Kept as a pure function ([`translate`]) so the App's reaction to a key
//! is independent of how crossterm reports it; tests in `crate::app` use
//! [`AppMsg`] directly and never need to fabricate crossterm events.
//!
//! Modal vs normal contexts route differently — inside [`Modal::ConfirmDelete`]
//! `y`/`n` are routed to `Confirm`/`Cancel`; outside, `n` opens a new
//! session.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use crate::app::{AppMsg, Focus, Modal, Tab};
use crate::crons::FieldEdit;

/// Hit boxes for the renderer's interactive regions, so mouse clicks can
/// be routed to the right [`AppMsg`].
///
/// `tabs` is the (tab, column_range) list returned by
/// [`crate::tabs::render_tabs`]; `sidebar` is the sidebar's **inner** content
/// rect — the area *inside* the borders, so the translator does not need to
/// special-case the border row. `sidebar_scroll_offset` is the list's
/// top-of-viewport index from the previous render, used to map a visible
/// row to the absolute index inside [`crate::sidebar::SidebarState::items`]
/// when the list has scrolled past its visible height.
pub struct HitBoxes {
    pub tabs: Vec<(Tab, std::ops::Range<u16>)>,
    pub sidebar: Rect,
    pub sidebar_scroll_offset: usize,
    pub tab_bar_row: u16,
    /// Active tab — drives which AppMsg variants the translator emits
    /// for shared keys (`j`/`k`/`n`/`d`/`r`/space). Without this the
    /// Crons tab would receive `SidebarDown` instead of `CronListDown`.
    pub tab: Tab,
    /// Focus inside the active tab. On Crons, `Focus::Main` means the
    /// editor pane has the cursor and printable keys feed
    /// [`FieldEdit::Insert`] instead of triggering navigation.
    pub focus: Focus,
}

/// Translate one crossterm event to an `AppMsg`, given the modal currently
/// open (if any) and the on-screen hit boxes. Returns `None` for events
/// the App does not care about (resize is handled by the runner directly,
/// key releases on terminals that send them, etc.).
pub fn translate(event: Event, modal: Option<&Modal>, hit: &HitBoxes) -> Option<AppMsg> {
    match event {
        Event::Key(k) => {
            // Some terminals (Windows in particular) report both press and
            // release; we only act on press to avoid double-firing.
            if k.kind != KeyEventKind::Press && k.kind != KeyEventKind::Repeat {
                return None;
            }
            translate_key(k.code, k.modifiers, modal, hit.tab, hit.focus)
        }
        Event::Mouse(m) => translate_mouse(m, hit),
        _ => None,
    }
}

fn translate_key(
    code: KeyCode,
    mods: KeyModifiers,
    modal: Option<&Modal>,
    tab: Tab,
    focus: Focus,
) -> Option<AppMsg> {
    // Modal-context keys first; otherwise normal navigation.
    if let Some(m) = modal {
        return translate_modal_key(code, mods, m);
    }
    // Crons-tab keys take priority when the user is on that tab —
    // otherwise `j` on the Crons list would still send SidebarDown and
    // the cron list wouldn't move.
    if tab == Tab::Crons {
        if let Some(msg) = translate_crons_key(code, mods, focus) {
            return Some(msg);
        }
        // Fall through to globals only (Tab / digit / q / ?). The
        // Sessions navigation keys are NOT advertised here.
        return translate_globals(code, mods);
    }
    Some(match code {
        // Always-on: quit / help / tab cycle.
        KeyCode::Char('q') => AppMsg::Quit,
        KeyCode::Char('?') => AppMsg::ToggleFullHints,
        KeyCode::Tab => AppMsg::NextTab,
        KeyCode::BackTab => AppMsg::PrevTab,
        KeyCode::Char('1') => AppMsg::SetTab(Tab::Sessions),
        KeyCode::Char('2') => AppMsg::SetTab(Tab::Crons),

        // Sidebar navigation.
        KeyCode::Char('j') | KeyCode::Down => AppMsg::SidebarDown,
        KeyCode::Char('k') | KeyCode::Up => AppMsg::SidebarUp,
        KeyCode::Char('h') | KeyCode::Left => AppMsg::SidebarCollapse,
        KeyCode::Char('l') | KeyCode::Right => AppMsg::SidebarExpand,
        KeyCode::Char('g') => AppMsg::SidebarTop,
        KeyCode::Char('G') => AppMsg::SidebarBottom,

        // Actions.
        KeyCode::Enter => AppMsg::Enter,
        KeyCode::Char('d') => AppMsg::Delete,
        KeyCode::Char('a') => AppMsg::ArchiveOrRestore,
        // WEK-101: uppercase `N` (Shift+N) opens the New-project
        // modal. Lowercase `n` MUST stay bound to NewSession — the
        // issue brief is explicit: 触发键为大写 `N`,小写 `n` 不应触发.
        // crossterm reports Shift+N as `Char('N')`; some terminals
        // also include the SHIFT modifier, so accept either form.
        KeyCode::Char('N') => AppMsg::NewProject,
        KeyCode::Char('n') => AppMsg::NewSession,
        KeyCode::Char('i') => AppMsg::ImportDiscovered,
        KeyCode::Char('f') => AppMsg::OpenErrors,
        KeyCode::Esc => AppMsg::Cancel,

        // WEK-42 / M4.3: uppercase UI-pref cycles. Capitals chosen so
        // they don't shadow the lowercase Sessions actions (`d`, `n`)
        // and feel discoverable as "shift = global preference toggle".
        KeyCode::Char('T') => AppMsg::CycleTheme,
        KeyCode::Char('H') => AppMsg::CycleKeyHints,
        KeyCode::Char('C') => AppMsg::ToggleCompact,

        // Ctrl-C is treated as Quit so muscle memory works even when the
        // modal swallows everything else.
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => AppMsg::Quit,

        _ => return None,
    })
}

/// Keys valid on the Crons tab. Two sub-contexts:
/// - `focus == Sidebar` → cron list navigation (`j`/`k`/`n`/`d`/`r`/space).
/// - `focus == Main` → form editor: printable chars feed the field, the
///   navigation keys are only the ones safe in a buffer (Tab/Shift-Tab/Esc).
fn translate_crons_key(code: KeyCode, mods: KeyModifiers, focus: Focus) -> Option<AppMsg> {
    // Globals first so Ctrl-C / Tab / digit / ? / q always win.
    if let KeyCode::Char('c') = code {
        if mods.contains(KeyModifiers::CONTROL) {
            return Some(AppMsg::Quit);
        }
    }
    // Ctrl-S commits the editor draft from either focus — saves the user
    // from hunting for a binding when the form is "obviously done".
    if let KeyCode::Char('s') = code {
        if mods.contains(KeyModifiers::CONTROL) {
            return Some(AppMsg::CronSaveDraft);
        }
    }
    if focus == Focus::Sidebar {
        return Some(match code {
            KeyCode::Char('q') => AppMsg::Quit,
            KeyCode::Char('?') => AppMsg::ToggleFullHints,
            KeyCode::Tab => AppMsg::CronFocusEditor,
            KeyCode::BackTab => AppMsg::PrevTab,
            KeyCode::Char('1') => AppMsg::SetTab(Tab::Sessions),
            KeyCode::Char('2') => AppMsg::SetTab(Tab::Crons),

            KeyCode::Char('j') | KeyCode::Down => AppMsg::CronListDown,
            KeyCode::Char('k') | KeyCode::Up => AppMsg::CronListUp,
            KeyCode::Char('g') => AppMsg::CronListTop,
            KeyCode::Char('G') => AppMsg::CronListBottom,

            KeyCode::Char('n') => AppMsg::CronNew,
            KeyCode::Char('d') => AppMsg::CronDelete,
            KeyCode::Char('f') => AppMsg::OpenErrors,
            KeyCode::Char(' ') => AppMsg::CronToggleEnabled,
            KeyCode::Char('r') => AppMsg::CronTriggerNow,
            KeyCode::Char('R') => AppMsg::CronDryRun,
            KeyCode::Char('i') | KeyCode::Char('e') | KeyCode::Enter => AppMsg::CronFocusEditor,
            KeyCode::Esc => AppMsg::Cancel,
            _ => return None,
        });
    }
    // Editor pane: printable chars → field edit; only structural keys
    // are reserved. `Enter` does NOT save unconditionally — the App
    // decides (CronEditorEnter inserts a newline on multi-line fields,
    // saves on single-line). Explicit "save now" stays on Ctrl+S so the
    // multi-line fields are still typable.
    //
    // WEK-42 / M4.3 scope decision: the editor is a free-typing context,
    // so the global `T` / `H` / `C` UI-pref keys are INTENTIONALLY
    // captured as field input here. Reviewer flagged the asymmetry; the
    // alternative (chording Ctrl+T/H/C to escape) collides with terminal
    // conventions (Ctrl+H = backspace, Ctrl+C already = Quit). The
    // user-visible workflow is: Esc → list → press T/H/C → continue
    // editing. This matches how every other editor handles modeful
    // prefs (vim's `:set`, less's `-/`). Pinned by
    // `crons_editor_swallows_t_h_c_intentionally` in key_hints tests so
    // the next reviewer doesn't have to spot it again.
    Some(match code {
        KeyCode::Esc => AppMsg::CronCancelDraft,
        KeyCode::Tab => AppMsg::CronFieldNext,
        KeyCode::BackTab => AppMsg::CronFieldPrev,
        KeyCode::Enter => AppMsg::CronEditorEnter,
        KeyCode::Backspace => AppMsg::CronFieldEdit(FieldEdit::Backspace),
        KeyCode::Char(c) => AppMsg::CronFieldEdit(FieldEdit::Insert(c)),
        _ => return None,
    })
}

/// The minimum global set every context honours: quit / help / tab
/// switch / UI prefs.
fn translate_globals(code: KeyCode, mods: KeyModifiers) -> Option<AppMsg> {
    Some(match code {
        KeyCode::Char('q') => AppMsg::Quit,
        KeyCode::Char('?') => AppMsg::ToggleFullHints,
        KeyCode::Tab => AppMsg::NextTab,
        KeyCode::BackTab => AppMsg::PrevTab,
        KeyCode::Char('1') => AppMsg::SetTab(Tab::Sessions),
        KeyCode::Char('2') => AppMsg::SetTab(Tab::Crons),
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => AppMsg::Quit,
        // WEK-42 / M4.3: uppercase T/H/C are the only chord-free
        // globals not already claimed by the Sessions / Crons tabs.
        // Lowercase t/h/c would collide with vim-style navigation
        // (`h` = fold, `t` is reserved for future inline filter).
        KeyCode::Char('T') => AppMsg::CycleTheme,
        KeyCode::Char('H') => AppMsg::CycleKeyHints,
        KeyCode::Char('C') => AppMsg::ToggleCompact,
        _ => return None,
    })
}

fn translate_modal_key(code: KeyCode, mods: KeyModifiers, modal: &Modal) -> Option<AppMsg> {
    // Ctrl-C always quits, even inside a modal.
    if let KeyCode::Char('c') = code {
        if mods.contains(KeyModifiers::CONTROL) {
            return Some(AppMsg::Quit);
        }
    }
    // WEK-42 / M4.3 review feedback: T/H/C are advertised as "globals" in
    // the `?` overlay, so they must work inside modals too — otherwise
    // a user who opens FullHints to read the binding can't actually try
    // it. Capital letters are unambiguous (no modal handler takes them
    // as confirm/cancel) so route them up to the global pref handlers
    // before the per-modal switch claims the keystroke.
    if let KeyCode::Char(c @ ('T' | 'H' | 'C')) = code {
        return Some(match c {
            'T' => AppMsg::CycleTheme,
            'H' => AppMsg::CycleKeyHints,
            _ => AppMsg::ToggleCompact,
        });
    }
    // WEK-94 / A2: NewSession owns its own field-edit routing — handled
    // up-front so the closed-form match below can stay one-msg-per-arm.
    if let Modal::NewSession(draft) = modal {
        return translate_new_session_key(code, mods, draft);
    }
    // WEK-101: NewProject is a free-typing modal too — paths can
    // legitimately contain digits, dashes, dots, and slashes that
    // would otherwise hit the "quit / tab / cancel" matrix.
    if let Modal::NewProject(_) = modal {
        return translate_new_project_key(code, mods);
    }
    Some(match modal {
        Modal::ConfirmDelete { .. } => match code {
            KeyCode::Char('y') | KeyCode::Enter => AppMsg::Confirm,
            KeyCode::Char('n') | KeyCode::Esc => AppMsg::Cancel,
            _ => return None,
        },
        Modal::FullHints => match code {
            KeyCode::Char('?') => AppMsg::ToggleFullHints,
            KeyCode::Esc | KeyCode::Char('q') => AppMsg::Cancel,
            _ => return None,
        },
        Modal::NewSession(_) => unreachable!("handled above"),
        Modal::NewProject(_) => unreachable!("handled above"),
        Modal::ConfirmEnableCron { .. } | Modal::ConfirmDeleteCron { .. } => match code {
            KeyCode::Char('y') | KeyCode::Enter => AppMsg::Confirm,
            KeyCode::Char('n') | KeyCode::Esc => AppMsg::Cancel,
            _ => return None,
        },
        Modal::DryRunCron { .. } => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => AppMsg::Cancel,
            _ => return None,
        },
        Modal::Errors { .. } => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('f') => {
                AppMsg::Cancel
            }
            _ => return None,
        },
    })
}

/// Modal-key translator for [`Modal::NewSession`]. Split out of
/// [`translate_modal_key`] because the form has its own field-edit
/// matrix that doesn't fit the simple "one keystroke → confirm/cancel"
/// shape used by the other modals.
fn translate_new_session_key(
    code: KeyCode,
    mods: KeyModifiers,
    _draft: &crate::app::NewSessionDraft,
) -> Option<AppMsg> {
    // Enter confirms from any field (Ctrl+Enter too — modifiers are
    // ignored here). Matches the advertised "⏎ create" hint.
    if let KeyCode::Enter = code {
        return Some(AppMsg::Confirm);
    }
    match code {
        KeyCode::Esc => Some(AppMsg::Cancel),
        KeyCode::Tab => Some(AppMsg::NewSessionFocusNext),
        KeyCode::BackTab => Some(AppMsg::NewSessionFocusPrev),
        // ←/→ + Space are translated unconditionally so the `?` overlay's
        // hint list survives the cross-check in
        // `every_advertised_modal_key_is_translatable`. The App's
        // `try_apply_new_session_field` is the authority on which field
        // actually reacts.
        KeyCode::Left => Some(AppMsg::NewSessionBackendPrev),
        KeyCode::Right => Some(AppMsg::NewSessionBackendNext),
        KeyCode::Char(' ') => Some(AppMsg::NewSessionToggleWorktree),
        _ => {
            let _ = mods;
            None
        }
    }
}

/// Modal-key translator for [`Modal::NewProject`] (WEK-101).
///
/// The buffer is a free-typing path field, so we must NOT route plain
/// `q` / `?` / digits / letters back through the normal "quit / help"
/// matrix — they're literal path characters. Only structural keys
/// (Enter / Esc / Tab / Up / Down / Backspace) carry control semantics.
///
/// Key map (hint == real binding — pinned by the modal hint test in
/// `crate::key_hints::tests`):
///   * Enter — Confirm. The App's Confirm arm "applies-then-confirms"
///     when a candidate is highlighted, so Enter on a fresh dropdown
///     selection descends one level first.
///   * Esc — Cancel (close modal).
///   * Tab / ↓ — cycle to the next candidate.
///   * Shift-Tab / ↑ — cycle to the previous candidate.
///   * → — apply highlighted candidate (descend without confirming).
///   * Backspace — pop last char from buffer.
///   * Ctrl+C — Quit (already intercepted at the top of
///     `translate_modal_key`; no special handling here).
///   * Any printable char — append to buffer (including digits and
///     letters that would otherwise be globals).
fn translate_new_project_key(code: KeyCode, mods: KeyModifiers) -> Option<AppMsg> {
    match code {
        KeyCode::Enter => Some(AppMsg::Confirm),
        KeyCode::Esc => Some(AppMsg::Cancel),
        KeyCode::Tab => Some(AppMsg::NewProjectCandidateNext),
        KeyCode::BackTab => Some(AppMsg::NewProjectCandidatePrev),
        KeyCode::Down => Some(AppMsg::NewProjectCandidateNext),
        KeyCode::Up => Some(AppMsg::NewProjectCandidatePrev),
        KeyCode::Right => Some(AppMsg::NewProjectApplyCandidate),
        KeyCode::Backspace => Some(AppMsg::NewProjectPathBackspace),
        KeyCode::Char(c) => {
            // Control-modified chords (Ctrl+S, Ctrl+R, …) MUST NOT
            // leak into the path buffer — they're terminal chrome.
            // Ctrl+C is already intercepted above by translate_modal_key.
            if mods.contains(KeyModifiers::CONTROL) {
                return None;
            }
            Some(AppMsg::NewProjectPathChar(c))
        }
        _ => None,
    }
}

fn translate_mouse(event: crossterm::event::MouseEvent, hit: &HitBoxes) -> Option<AppMsg> {
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Tab bar click.
            if event.row == hit.tab_bar_row {
                if let Some(t) = crate::tabs::tab_at_column(event.column, &hit.tabs) {
                    return Some(AppMsg::SetTab(t));
                }
            }
            // Sidebar row click: `hit.sidebar` is already the inner area
            // (borders excluded by the renderer), so a click inside it
            // maps directly to a visible-row offset. Add the list's
            // scroll offset to get the absolute index in `items` — the
            // ratatui `List` widget auto-scrolls long lists and without
            // this correction `d`/`a` would target the wrong row.
            if hit
                .sidebar
                .contains(ratatui::layout::Position::new(event.column, event.row))
            {
                let inner_row = event.row.saturating_sub(hit.sidebar.y) as usize;
                let abs_index = hit.sidebar_scroll_offset.saturating_add(inner_row);
                return Some(AppMsg::SidebarSelect(abs_index));
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseEvent};

    fn hit() -> HitBoxes {
        HitBoxes {
            tabs: vec![(Tab::Sessions, 0u16..10u16), (Tab::Crons, 10u16..20u16)],
            // Pre-trimmed inner rect (borders already removed by the
            // renderer). y=2 ⇒ first visible row is at terminal row 2.
            sidebar: Rect::new(1, 2, 28, 8),
            sidebar_scroll_offset: 0,
            tab_bar_row: 0,
            tab: Tab::Sessions,
            focus: Focus::Sidebar,
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn j_in_normal_context_is_sidebar_down() {
        let msg = translate(key(KeyCode::Char('j')), None, &hit());
        assert_eq!(msg, Some(AppMsg::SidebarDown));
    }

    #[test]
    fn n_normal_creates_session_but_modal_n_cancels() {
        let msg_normal = translate(key(KeyCode::Char('n')), None, &hit());
        assert_eq!(msg_normal, Some(AppMsg::NewSession));

        let modal = Modal::ConfirmDelete {
            session_id: "x".into(),
        };
        let msg_modal = translate(key(KeyCode::Char('n')), Some(&modal), &hit());
        assert_eq!(msg_modal, Some(AppMsg::Cancel));
    }

    #[test]
    fn y_in_delete_modal_confirms() {
        let modal = Modal::ConfirmDelete {
            session_id: "x".into(),
        };
        assert_eq!(
            translate(key(KeyCode::Char('y')), Some(&modal), &hit()),
            Some(AppMsg::Confirm)
        );
    }

    #[test]
    fn ctrl_c_quits_even_in_modal() {
        let modal = Modal::ConfirmDelete {
            session_id: "x".into(),
        };
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(translate(ev, Some(&modal), &hit()), Some(AppMsg::Quit));
    }

    #[test]
    fn left_click_on_tab_bar_routes_to_set_tab() {
        let ev = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            translate(ev, None, &hit()),
            Some(AppMsg::SetTab(Tab::Crons))
        );
    }

    #[test]
    fn left_click_inside_sidebar_routes_to_select_index() {
        let ev = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5, // sidebar inner y=2; row 5 ⇒ inner_row = 3
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(translate(ev, None, &hit()), Some(AppMsg::SidebarSelect(3)));
    }

    #[test]
    fn left_click_inside_sidebar_compensates_for_scroll_offset() {
        // Long list scenario: the renderer scrolled the list by 12 rows
        // before drawing. A click on the first visible row must resolve to
        // item index 12, not 0 — otherwise `d`/`a` hit the wrong session
        // when the user is operating in the lower half of a tall list.
        let h = HitBoxes {
            tabs: vec![(Tab::Sessions, 0u16..10u16), (Tab::Crons, 10u16..20u16)],
            sidebar: Rect::new(1, 2, 28, 8),
            sidebar_scroll_offset: 12,
            tab_bar_row: 0,
            tab: Tab::Sessions,
            focus: Focus::Sidebar,
        };
        let ev = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4, // inner_row = 2; absolute = 12 + 2 = 14
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(translate(ev, None, &h), Some(AppMsg::SidebarSelect(14)));
    }

    #[test]
    fn left_click_on_sidebar_border_is_outside_inner_hit_box() {
        // Border row sits one row above the inner hit box (y=1 vs y=2)
        // and must not snap the selection to item 0.
        let ev = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(translate(ev, None, &hit()), None);
    }

    #[test]
    fn question_mark_toggles_full_hints() {
        assert_eq!(
            translate(key(KeyCode::Char('?')), None, &hit()),
            Some(AppMsg::ToggleFullHints)
        );
    }

    #[test]
    fn esc_cancels_normal_context() {
        assert_eq!(
            translate(key(KeyCode::Esc), None, &hit()),
            Some(AppMsg::Cancel)
        );
    }

    #[test]
    fn key_release_is_ignored() {
        let mut k = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert_eq!(translate(Event::Key(k), None, &hit()), None);
    }

    // ---- WEK-101: Shift+N opens NewProject, lowercase n stays NewSession ----

    #[test]
    fn lowercase_n_still_creates_session_not_project() {
        // Issue brief: 触发键为大写 `N`,小写 `n` 不应触发.
        // Pin both halves so a future remap can't silently regress.
        let msg = translate(key(KeyCode::Char('n')), None, &hit());
        assert_eq!(msg, Some(AppMsg::NewSession));
    }

    #[test]
    fn uppercase_n_opens_new_project_modal() {
        // crossterm reports Shift+N as `Char('N')` even when the SHIFT
        // modifier is also set — accept either form so the binding is
        // robust across terminals.
        let plain_n = translate(key(KeyCode::Char('N')), None, &hit());
        assert_eq!(plain_n, Some(AppMsg::NewProject));
        let with_shift = translate(
            Event::Key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            None,
            &hit(),
        );
        assert_eq!(with_shift, Some(AppMsg::NewProject));
    }

    #[test]
    fn new_project_modal_routes_printable_chars_into_buffer_not_quit() {
        // The path buffer must accept `q`, `?`, digits, `n` literally —
        // they would otherwise fire global Quit / Help / NewSession.
        let modal = Modal::NewProject(crate::app::NewProjectDraft::new());
        assert_eq!(
            translate(key(KeyCode::Char('q')), Some(&modal), &hit()),
            Some(AppMsg::NewProjectPathChar('q'))
        );
        assert_eq!(
            translate(key(KeyCode::Char('?')), Some(&modal), &hit()),
            Some(AppMsg::NewProjectPathChar('?'))
        );
        assert_eq!(
            translate(key(KeyCode::Char('5')), Some(&modal), &hit()),
            Some(AppMsg::NewProjectPathChar('5'))
        );
        assert_eq!(
            translate(key(KeyCode::Char('n')), Some(&modal), &hit()),
            Some(AppMsg::NewProjectPathChar('n'))
        );
    }

    #[test]
    fn new_project_modal_structural_keys() {
        let modal = Modal::NewProject(crate::app::NewProjectDraft::new());
        assert_eq!(
            translate(key(KeyCode::Tab), Some(&modal), &hit()),
            Some(AppMsg::NewProjectCandidateNext)
        );
        assert_eq!(
            translate(key(KeyCode::BackTab), Some(&modal), &hit()),
            Some(AppMsg::NewProjectCandidatePrev)
        );
        assert_eq!(
            translate(key(KeyCode::Down), Some(&modal), &hit()),
            Some(AppMsg::NewProjectCandidateNext)
        );
        assert_eq!(
            translate(key(KeyCode::Up), Some(&modal), &hit()),
            Some(AppMsg::NewProjectCandidatePrev)
        );
        assert_eq!(
            translate(key(KeyCode::Right), Some(&modal), &hit()),
            Some(AppMsg::NewProjectApplyCandidate)
        );
        assert_eq!(
            translate(key(KeyCode::Enter), Some(&modal), &hit()),
            Some(AppMsg::Confirm)
        );
        assert_eq!(
            translate(key(KeyCode::Esc), Some(&modal), &hit()),
            Some(AppMsg::Cancel)
        );
        assert_eq!(
            translate(key(KeyCode::Backspace), Some(&modal), &hit()),
            Some(AppMsg::NewProjectPathBackspace)
        );
    }

    #[test]
    fn ctrl_c_still_quits_inside_new_project_modal() {
        // Modal-level safety: Ctrl+C must always escape, even when every
        // printable char goes into the path buffer.
        let modal = Modal::NewProject(crate::app::NewProjectDraft::new());
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(translate(ev, Some(&modal), &hit()), Some(AppMsg::Quit));
    }
}
