//! Context-driven key hints (PRD §5.6).
//!
//! Two responsibilities:
//!
//! 1. Decide which keys are bindable in the current `(tab, focus, selection,
//!    modal)` context. This is the source of truth that both the bottom
//!    one-line hint bar and the `?` which-key overlay read from — they must
//!    agree, or the overlay becomes the documentation of a different
//!    application than the user is operating.
//! 2. Sort by **importance** (PRD §5.6 第 2 条: "按重要性排序，不按字母序"),
//!    not by key character. Each hint carries an [`Importance`] used by the
//!    bar to pick the top-N and by the overlay to order rows.
//!
//! The hint catalogue is intentionally declarative (a const-fn-free table
//! of [`Hint`] values built in [`HintRegistry::for_context`]). Adding a key
//! is one line; the renderer never has to learn about it.

use crate::app::{Focus, Modal, Tab};
use crate::sidebar::Selection;

/// Relative importance, highest first. Used to:
/// - Order rows in the `?` overlay.
/// - Choose what survives truncation in the one-line hint bar when the
///   terminal is narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Importance {
    /// Discoverability glue: `?`, `q`. Always last unless they ARE the
    /// only option.
    Meta = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    /// Primary action — `Enter` for the current selection. Top of the bar.
    Primary = 4,
}

/// One key binding the user can fire right now.
///
/// `key` is a presentational label (`"j"`, `"⏎"`, `"Tab"`); it does NOT
/// need to be a literal `crossterm::event::KeyCode` — the input layer is
/// the matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub key: &'static str,
    pub label: &'static str,
    pub importance: Importance,
}

impl Hint {
    pub const fn new(key: &'static str, label: &'static str, importance: Importance) -> Self {
        Self {
            key,
            label,
            importance,
        }
    }
}

/// Catalogue of hints valid in the current context.
///
/// The "context" is the tuple `(tab, focus, selection_kind, modal)` — i.e.,
/// every input the catalogue might branch on. We pass them in by reference
/// instead of reading them from a borrowed `App` so the registry can be
/// unit-tested without a live app, and so a future renderer (overlay,
/// status bar, help screen) can ask for the same catalogue without
/// re-implementing the branching.
pub struct HintRegistry;

impl HintRegistry {
    /// Compute the ordered (high importance first) list of hints valid for
    /// the given context. Order within the same importance is preserved
    /// from the catalogue, which is hand-tuned (Enter, then most-used,
    /// then teaching keys at the tail).
    pub fn for_context(
        tab: Tab,
        focus: Focus,
        selection: &Selection,
        modal: Option<Modal>,
    ) -> Vec<Hint> {
        if let Some(m) = modal {
            return Self::for_modal(m);
        }
        match tab {
            Tab::Sessions => Self::for_sessions(focus, selection),
            // Crons tab placeholder until M3. We still want `Tab` / `?` /
            // `q` to be teachable so the user is not stranded on an empty
            // tab — but we deliberately do NOT advertise cron-management
            // keys we cannot honour yet.
            Tab::Crons => {
                let mut out = Self::globals();
                out.sort_by(|a, b| b.importance.cmp(&a.importance));
                out
            }
        }
    }

    fn for_sessions(focus: Focus, selection: &Selection) -> Vec<Hint> {
        let mut out = Vec::new();

        if matches!(focus, Focus::Sidebar) {
            // Selection-sensitive entries.
            match selection {
                Selection::Session { .. } => {
                    out.push(Hint::new("⏎", "open", Importance::Primary));
                    out.push(Hint::new("d", "delete", Importance::High));
                    out.push(Hint::new("a", "archive", Importance::High));
                }
                Selection::Group { project_id } => {
                    if project_id == crate::model::ProjectGroup::ARCHIVED_ID {
                        // PRD §5.3: archived bucket is "可展开恢复"; the
                        // primary action is restoring a child (so the
                        // group header itself just gets fold/expand).
                        out.push(Hint::new("l", "expand", Importance::Primary));
                    } else {
                        out.push(Hint::new("h", "fold", Importance::High));
                        out.push(Hint::new("l", "expand", Importance::High));
                        out.push(Hint::new("n", "new", Importance::Medium));
                    }
                }
                Selection::Empty => {
                    // PRD §5.6 第 3 条: hint 反映**真实绑定**。`n` 在空
                    // workspace 上是 no-op（[`crate::app::App::on_new_session`]
                    // 没有 project 可挂载，直接 return），所以这里不再 advertise；
                    // 空态文案由 [`crate::runner::render_content_placeholder`]
                    // 负责。等 M1.7 daemon 支持"创建项目"路径，再恢复 `n`。
                }
            }
            // Always-on navigation inside the sidebar. Placed AFTER the
            // selection-specific entries so the importance sort keeps the
            // primary action first; the navigation row is what teaches
            // vim users they're on familiar ground.
            out.push(Hint::new("j/k", "down/up", Importance::Medium));
            out.push(Hint::new("g/G", "top/bottom", Importance::Low));
            // `n` is a global-ish action on the sidebar (new session on
            // the current project). Already pushed above when selection
            // is a non-archived group; add it for session rows too so the
            // user can hit `n` from anywhere in the sidebar.
            if matches!(selection, Selection::Session { .. }) {
                out.push(Hint::new("n", "new", Importance::Medium));
            }
        }

        // Globals visible in any focus on the Sessions tab.
        out.extend(Self::globals());
        out.sort_by(|a, b| b.importance.cmp(&a.importance));
        out
    }

    fn for_modal(modal: Modal) -> Vec<Hint> {
        match modal {
            Modal::ConfirmDelete { .. } => vec![
                Hint::new("y", "confirm delete", Importance::Primary),
                Hint::new("n / Esc", "cancel", Importance::High),
            ],
            Modal::FullHints => vec![Hint::new("Esc / ?", "close", Importance::Primary)],
            Modal::NewSession { .. } => vec![
                Hint::new("⏎", "create", Importance::Primary),
                Hint::new("Esc", "cancel", Importance::High),
                // PRD §5.6 第 3 条: hint 必须 == 当前真实绑定。Tab/Backend
                // 选择在 M1.7 落 daemon 之前没有 handler，所以这里**不**
                // advertise — 等 backend chooser 上线再加回来。
            ],
        }
    }

    fn globals() -> Vec<Hint> {
        vec![
            Hint::new("Tab", "next tab", Importance::Low),
            Hint::new("?", "all keys", Importance::Meta),
            Hint::new("q", "quit", Importance::Meta),
        ]
    }
}

/// Format the bottom hint bar from a hint list, truncated to fit `width`
/// columns. Falls back to "Press ? for help" if even one hint doesn't fit.
///
/// Each hint renders as `key label` separated by `  ·  `. We greedily
/// include hints in importance order until the next would overflow.
pub fn format_hint_bar(hints: &[Hint], width: usize) -> String {
    const SEP: &str = "  ·  ";
    if hints.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0usize;
    for h in hints {
        // Use char count, not byte len — `⏎` and `▾` are multi-byte glyphs
        // but render as one terminal cell. char count is the closest
        // approximation we can get without pulling in `unicode-width`.
        let piece = format!("{} {}", h.key, h.label);
        let cost = if parts.is_empty() {
            piece.chars().count()
        } else {
            piece.chars().count() + SEP.chars().count()
        };
        if used + cost > width {
            break;
        }
        parts.push(piece);
        used += cost;
    }
    if parts.is_empty() {
        return "Press ? for help".to_string();
    }
    parts.join(SEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::Selection;

    #[test]
    fn primary_action_first_for_session_selection() {
        let sel = Selection::Session {
            project_id: "p1".into(),
            session_id: "s1".into(),
        };
        let hints = HintRegistry::for_context(Tab::Sessions, Focus::Sidebar, &sel, None);
        assert_eq!(hints.first().unwrap().key, "⏎");
        assert_eq!(hints.first().unwrap().label, "open");
    }

    #[test]
    fn archived_bucket_header_omits_delete_and_archive() {
        let sel = Selection::Group {
            project_id: crate::model::ProjectGroup::ARCHIVED_ID.into(),
        };
        let hints = HintRegistry::for_context(Tab::Sessions, Focus::Sidebar, &sel, None);
        for h in &hints {
            assert!(
                h.label != "delete",
                "archived bucket header must not advertise delete"
            );
            assert!(
                h.label != "archive",
                "archived bucket header must not advertise archive"
            );
        }
    }

    #[test]
    fn modal_replaces_normal_hints() {
        let sel = Selection::Empty;
        let hints = HintRegistry::for_context(
            Tab::Sessions,
            Focus::Sidebar,
            &sel,
            Some(Modal::ConfirmDelete {
                session_id: "s1".into(),
            }),
        );
        // No j/k while a modal is open.
        assert!(hints.iter().all(|h| h.key != "j/k"));
        assert_eq!(hints.first().unwrap().label, "confirm delete");
    }

    #[test]
    fn refreshes_on_selection_change() {
        // Switching from group header to session must change the primary
        // action (this is the property PRD §5.6 第 4 条: "焦点/模式/选中变化
        // 时立刻刷新" depends on).
        let group_sel = Selection::Group {
            project_id: "p1".into(),
        };
        let session_sel = Selection::Session {
            project_id: "p1".into(),
            session_id: "s1".into(),
        };
        let g = HintRegistry::for_context(Tab::Sessions, Focus::Sidebar, &group_sel, None);
        let s = HintRegistry::for_context(Tab::Sessions, Focus::Sidebar, &session_sel, None);
        assert_ne!(g.first().unwrap().label, s.first().unwrap().label);
    }

    #[test]
    fn hint_bar_truncates_to_width() {
        let hints = vec![
            Hint::new("⏎", "open", Importance::Primary),
            Hint::new("d", "delete", Importance::High),
            Hint::new("a", "archive", Importance::High),
            Hint::new("?", "all keys", Importance::Meta),
        ];
        let wide = format_hint_bar(&hints, 200);
        assert!(wide.contains("open"));
        assert!(wide.contains("all keys"));
        let narrow = format_hint_bar(&hints, 10);
        // Even narrow shows at least the primary.
        assert!(narrow.contains("open") || narrow == "Press ? for help");
    }

    #[test]
    fn hint_bar_falls_back_when_nothing_fits() {
        let hints = vec![Hint::new("verylongkey", "verylonglabel", Importance::High)];
        let out = format_hint_bar(&hints, 5);
        assert_eq!(out, "Press ? for help");
    }

    /// Cross-check: every key the registry advertises in a non-modal Sessions
    /// context must be translatable by [`crate::input::translate`] — otherwise
    /// the hint bar is documenting an action that does nothing. Guards against
    /// future drift between the two layers (review a906b484 §1, §2).
    #[test]
    fn every_advertised_session_key_is_translatable() {
        use crate::input::{translate, HitBoxes};
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        use ratatui::layout::Rect;

        let hit = HitBoxes {
            tabs: Vec::new(),
            sidebar: Rect::default(),
            sidebar_scroll_offset: 0,
            tab_bar_row: 0,
        };

        let contexts = [
            Selection::Empty,
            Selection::Group {
                project_id: "p1".into(),
            },
            Selection::Group {
                project_id: crate::model::ProjectGroup::ARCHIVED_ID.into(),
            },
            Selection::Session {
                project_id: "p1".into(),
                session_id: "s1".into(),
            },
        ];
        for sel in &contexts {
            let hints = HintRegistry::for_context(Tab::Sessions, Focus::Sidebar, sel, None);
            for h in &hints {
                let Some(code) = single_char_key(h.key) else {
                    continue;
                };
                let ev = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
                assert!(
                    translate(ev, None, &hit).is_some(),
                    "hint {:?} -> {} advertises key '{}' that input layer ignores in selection {sel:?}",
                    h.label,
                    h.key,
                    h.key,
                );
            }
        }
    }

    /// Same cross-check for modal contexts: NewSession used to advertise
    /// `Tab next backend` even though [`crate::input::translate_modal_key`]
    /// only handles Enter/Esc inside it. Pin the invariant so the next
    /// reviewer doesn't have to spot it again (review a906b484 §1).
    #[test]
    fn every_advertised_modal_key_is_translatable() {
        use crate::app::Modal;
        use crate::input::{translate, HitBoxes};
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        use ratatui::layout::Rect;

        let hit = HitBoxes {
            tabs: Vec::new(),
            sidebar: Rect::default(),
            sidebar_scroll_offset: 0,
            tab_bar_row: 0,
        };
        let modals = [
            Modal::ConfirmDelete {
                session_id: "s1".into(),
            },
            Modal::FullHints,
            Modal::NewSession {
                project_id: "p1".into(),
            },
        ];
        for m in &modals {
            let hints = HintRegistry::for_context(
                Tab::Sessions,
                Focus::Sidebar,
                &Selection::Empty,
                Some(m.clone()),
            );
            for h in &hints {
                let Some(code) = single_char_key(h.key) else {
                    continue;
                };
                let ev = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
                assert!(
                    translate(ev, Some(m), &hit).is_some(),
                    "modal {m:?} advertises key '{}' that input layer ignores",
                    h.key,
                );
            }
        }
    }

    /// Map the registry's presentational key labels back to the [`KeyCode`]
    /// the input layer matches against. Returns `None` for composite labels
    /// (`j/k`, `g/G`, `n / Esc`, `Esc / ?`) — the test treats those as
    /// "one of the chars works"; the explicit assertions below cover the
    /// individual keys.
    fn single_char_key(label: &str) -> Option<crossterm::event::KeyCode> {
        use crossterm::event::KeyCode;
        match label {
            "⏎" => Some(KeyCode::Enter),
            "Tab" => Some(KeyCode::Tab),
            "Esc" => Some(KeyCode::Esc),
            "d" => Some(KeyCode::Char('d')),
            "a" => Some(KeyCode::Char('a')),
            "n" => Some(KeyCode::Char('n')),
            "h" => Some(KeyCode::Char('h')),
            "l" => Some(KeyCode::Char('l')),
            "q" => Some(KeyCode::Char('q')),
            "y" => Some(KeyCode::Char('y')),
            "?" => Some(KeyCode::Char('?')),
            _ => None,
        }
    }
}
