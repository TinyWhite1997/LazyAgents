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

use crate::app::{AppMsg, Modal, Tab};

/// Hit boxes for the renderer's interactive regions, so mouse clicks can
/// be routed to the right [`AppMsg`].
///
/// `tabs` is the (tab, column_range) list returned by
/// [`crate::tabs::render_tabs`]; `sidebar` is the sidebar's inner content
/// rect (so we can compute the clicked row index).
pub struct HitBoxes {
    pub tabs: Vec<(Tab, std::ops::Range<u16>)>,
    pub sidebar: Rect,
    pub tab_bar_row: u16,
}

/// Translate one crossterm event to an `AppMsg`, given the modal currently
/// open (if any) and the on-screen hit boxes. Returns `None` for events
/// the App does not care about (resize is handled by the runner directly,
/// key releases on terminals that send them, etc.).
pub fn translate(
    event: Event,
    modal: Option<&Modal>,
    hit: &HitBoxes,
) -> Option<AppMsg> {
    match event {
        Event::Key(k) => {
            // Some terminals (Windows in particular) report both press and
            // release; we only act on press to avoid double-firing.
            if k.kind != KeyEventKind::Press && k.kind != KeyEventKind::Repeat {
                return None;
            }
            translate_key(k.code, k.modifiers, modal)
        }
        Event::Mouse(m) => translate_mouse(m, hit),
        _ => None,
    }
}

fn translate_key(
    code: KeyCode,
    mods: KeyModifiers,
    modal: Option<&Modal>,
) -> Option<AppMsg> {
    // Modal-context keys first; otherwise normal navigation.
    if let Some(m) = modal {
        return translate_modal_key(code, mods, m);
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
        KeyCode::Char('n') => AppMsg::NewSession,
        KeyCode::Esc => AppMsg::Cancel,

        // Ctrl-C is treated as Quit so muscle memory works even when the
        // modal swallows everything else.
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => AppMsg::Quit,

        _ => return None,
    })
}

fn translate_modal_key(
    code: KeyCode,
    mods: KeyModifiers,
    modal: &Modal,
) -> Option<AppMsg> {
    // Ctrl-C always quits, even inside a modal.
    if let KeyCode::Char('c') = code {
        if mods.contains(KeyModifiers::CONTROL) {
            return Some(AppMsg::Quit);
        }
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
        Modal::NewSession { .. } => match code {
            KeyCode::Enter => AppMsg::Confirm,
            KeyCode::Esc => AppMsg::Cancel,
            _ => return None,
        },
    })
}

fn translate_mouse(
    event: crossterm::event::MouseEvent,
    hit: &HitBoxes,
) -> Option<AppMsg> {
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Tab bar click.
            if event.row == hit.tab_bar_row {
                if let Some(t) = crate::tabs::tab_at_column(event.column, &hit.tabs) {
                    return Some(AppMsg::SetTab(t));
                }
            }
            // Sidebar row click: translate the row offset inside the
            // sidebar's content area to a list index. We assume the
            // sidebar is bordered (1-cell margin) — matches
            // `render_sidebar`'s Block::default().borders(ALL).
            if hit.sidebar.contains(ratatui::layout::Position::new(event.column, event.row)) {
                let inner_row = event.row.saturating_sub(hit.sidebar.y + 1);
                return Some(AppMsg::SidebarSelect(inner_row as usize));
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
            tabs: vec![
                (Tab::Sessions, 0u16..10u16),
                (Tab::Crons, 10u16..20u16),
            ],
            sidebar: Rect::new(0, 2, 30, 10),
            tab_bar_row: 0,
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
        assert_eq!(translate(ev, None, &hit()), Some(AppMsg::SetTab(Tab::Crons)));
    }

    #[test]
    fn left_click_inside_sidebar_routes_to_select_index() {
        let ev = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5, // sidebar at y=2 + inner_row(3 - 1 border) = absolute 5
            modifiers: KeyModifiers::NONE,
        });
        // sidebar y=2, border row at y=2; the click at row=5 ⇒ inner_row = 5-2-1 = 2
        assert_eq!(translate(ev, None, &hit()), Some(AppMsg::SidebarSelect(2)));
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
        assert_eq!(translate(key(KeyCode::Esc), None, &hit()), Some(AppMsg::Cancel));
    }

    #[test]
    fn key_release_is_ignored() {
        let mut k = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert_eq!(translate(Event::Key(k), None, &hit()), None);
    }
}
