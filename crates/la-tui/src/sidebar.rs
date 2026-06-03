//! Sessions sidebar — navigation state machine + ratatui widget.
//!
//! Two layers, kept separate so the navigation is unit-testable without a
//! terminal:
//!
//! 1. [`SidebarState`] — owns the flat "visible item list" (group headers +
//!    expanded children) and the cursor index into it. Every key event is a
//!    method on this struct that returns the new cursor's [`Selection`].
//!    No ratatui types appear here.
//! 2. [`render_sidebar`] — pure renderer: given a [`SidebarState`] and a
//!    [`ratatui::layout::Rect`], paint into a `Frame`. No state mutation.
//!
//! ## Why a precomputed flat list
//!
//! `j`/`k` and `h`/`l` are O(1) on a flat indexable list; the only mutation
//! is the cursor (and the `expanded` flag of a group on `h`/`l`). We
//! rebuild the flat list on data refresh (cheap: < 100 items per realistic
//! workspace) and on fold toggles, but never on cursor moves.

use std::cell::Cell;

use la_proto::notifications::BackendHealthStatus;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::model::{BackendBadge, ProjectGroup, RunState};

/// What the cursor is pointing at.
///
/// Returned by every navigation method so callers (the App layer) can
/// react: route `Enter` differently for a group header vs a session row,
/// disable `d` / `a` on the Archived bucket header, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Nothing visible (empty workspace).
    Empty,
    /// A group header. `project_id` is [`ProjectGroup::ARCHIVED_ID`] when
    /// the Archived bucket is selected.
    Group { project_id: String },
    /// A session row inside a group.
    Session {
        project_id: String,
        session_id: String,
    },
}

impl Selection {
    pub fn is_session(&self) -> bool {
        matches!(self, Selection::Session { .. })
    }
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Selection::Session { session_id, .. } => Some(session_id),
            _ => None,
        }
    }
    pub fn project_id(&self) -> Option<&str> {
        match self {
            Selection::Group { project_id } => Some(project_id),
            Selection::Session { project_id, .. } => Some(project_id),
            _ => None,
        }
    }
}

/// One row of the flattened sidebar view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    GroupHeader {
        group_index: usize,
    },
    SessionRow {
        group_index: usize,
        session_index: usize,
    },
}

/// Navigation state for the Sessions sidebar.
///
/// Owns the current data snapshot (groups), the per-group `expanded` flag,
/// and the cursor index into the flattened visible list. Replace the data
/// with [`SidebarState::set_groups`] when the source has new rows.
pub struct SidebarState {
    groups: Vec<ProjectGroup>,
    /// Flattened visible items in display order. Rebuilt by [`rebuild`].
    items: Vec<Item>,
    /// Index into `items`; `None` when `items` is empty.
    cursor: Option<usize>,
    /// First visible row (top-of-viewport) as reported by ratatui's
    /// `ListState::offset` after the last render. We mirror it here so the
    /// mouse-click translator can convert a screen-row inside the sidebar
    /// to the correct absolute index in `items` when the list has scrolled
    /// (PRD §5.3 keyboard is first-class, but mouse must not mis-target —
    /// `d` / `a` would otherwise hit the wrong session on long lists).
    ///
    /// Wrapped in [`Cell`] so the renderer can write it through a `&self`
    /// borrow without forcing the whole sidebar widget tree to be `&mut`.
    scroll_offset: Cell<usize>,
}

impl SidebarState {
    pub fn new() -> Self {
        Self {
            groups: Vec::new(),
            items: Vec::new(),
            cursor: None,
            scroll_offset: Cell::new(0),
        }
    }

    /// Replace the data snapshot. Preserves the per-group `expanded` flag
    /// from the previous snapshot when possible (so a refresh does NOT
    /// silently re-expand the Archived bucket the user just folded) and
    /// preserves the cursor's [`Selection`] when the same session still
    /// exists.
    pub fn set_groups(&mut self, mut new_groups: Vec<ProjectGroup>) {
        // Carry forward the expanded flag and the cursor's selection so a
        // refresh during navigation doesn't blow away the user's context.
        let prev_selection = self.selection();
        let prev_expanded: std::collections::HashMap<String, bool> = self
            .groups
            .iter()
            .map(|g| (g.project_id.clone(), g.expanded))
            .collect();
        for g in &mut new_groups {
            if let Some(prev) = prev_expanded.get(&g.project_id) {
                g.expanded = *prev;
            }
        }
        self.groups = new_groups;
        self.rebuild();
        self.cursor = self.find_selection_index(&prev_selection).or({
            if self.items.is_empty() {
                None
            } else {
                Some(0)
            }
        });
    }

    /// Direct accessor for renderers; do not mutate.
    pub fn groups(&self) -> &[ProjectGroup] {
        &self.groups
    }

    /// Number of visible items (group headers + expanded children).
    pub fn visible_len(&self) -> usize {
        self.items.len()
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// Last known top-of-viewport row index inside `items`. Returns 0 before
    /// the first render or whenever the list fits inside its area.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset.get()
    }

    /// Current [`Selection`] for the cursor.
    pub fn selection(&self) -> Selection {
        let Some(i) = self.cursor else {
            return Selection::Empty;
        };
        let Some(item) = self.items.get(i) else {
            return Selection::Empty;
        };
        match item {
            Item::GroupHeader { group_index } => Selection::Group {
                project_id: self.groups[*group_index].project_id.clone(),
            },
            Item::SessionRow {
                group_index,
                session_index,
            } => {
                let g = &self.groups[*group_index];
                Selection::Session {
                    project_id: g.project_id.clone(),
                    session_id: g.sessions[*session_index].session_id.clone(),
                }
            }
        }
    }

    // ---- navigation -------------------------------------------------

    /// Move the cursor down by one (vim `j`). Saturates at the last item.
    pub fn move_down(&mut self) -> Selection {
        if let Some(c) = self.cursor {
            if c + 1 < self.items.len() {
                self.cursor = Some(c + 1);
            }
        }
        self.selection()
    }

    /// Move the cursor up by one (vim `k`). Saturates at the first item.
    pub fn move_up(&mut self) -> Selection {
        if let Some(c) = self.cursor {
            if c > 0 {
                self.cursor = Some(c - 1);
            }
        }
        self.selection()
    }

    /// Jump to the first visible item (vim `g`).
    pub fn move_top(&mut self) -> Selection {
        if !self.items.is_empty() {
            self.cursor = Some(0);
        }
        self.selection()
    }

    /// Jump to the last visible item (vim `G`).
    pub fn move_bottom(&mut self) -> Selection {
        if !self.items.is_empty() {
            self.cursor = Some(self.items.len() - 1);
        }
        self.selection()
    }

    /// Collapse the enclosing group (`h`). When the cursor is on a session
    /// row, collapses the parent group and moves the cursor to that header
    /// so a subsequent `j` does not appear to "skip" rows. On a group
    /// header that is already collapsed, this is a no-op (it does NOT
    /// jump to the parent, since groups are at the top level).
    pub fn collapse(&mut self) -> Selection {
        let Some(c) = self.cursor else {
            return Selection::Empty;
        };
        let group_index = match self.items[c] {
            Item::GroupHeader { group_index } => group_index,
            Item::SessionRow { group_index, .. } => group_index,
        };
        if self.groups[group_index].expanded {
            self.groups[group_index].expanded = false;
            self.rebuild();
            // After rebuild the parent header's index is the one we want.
            self.cursor = self.items.iter().position(
                |it| matches!(it, Item::GroupHeader { group_index: g } if *g == group_index),
            );
        }
        self.selection()
    }

    /// Expand the current group or the parent group (`l`).
    pub fn expand(&mut self) -> Selection {
        let Some(c) = self.cursor else {
            return Selection::Empty;
        };
        let group_index = match self.items[c] {
            Item::GroupHeader { group_index } => group_index,
            Item::SessionRow { group_index, .. } => group_index,
        };
        if !self.groups[group_index].expanded {
            self.groups[group_index].expanded = true;
            self.rebuild();
            self.cursor = self.items.iter().position(
                |it| matches!(it, Item::GroupHeader { group_index: g } if *g == group_index),
            );
        }
        self.selection()
    }

    /// Select an item by absolute index (mouse click); ignored if out of
    /// range.
    pub fn select_index(&mut self, idx: usize) -> Selection {
        if idx < self.items.len() {
            self.cursor = Some(idx);
        }
        self.selection()
    }

    // ---- helpers ----------------------------------------------------

    /// Rebuild [`items`] from the current `groups` snapshot. Called whenever
    /// the data or a `expanded` flag changes.
    fn rebuild(&mut self) {
        self.items.clear();
        for (gi, g) in self.groups.iter().enumerate() {
            self.items.push(Item::GroupHeader { group_index: gi });
            if g.expanded {
                for si in 0..g.sessions.len() {
                    self.items.push(Item::SessionRow {
                        group_index: gi,
                        session_index: si,
                    });
                }
            }
        }
    }

    fn find_selection_index(&self, sel: &Selection) -> Option<usize> {
        match sel {
            Selection::Empty => None,
            Selection::Group { project_id } => self.items.iter().position(|it| {
                let Item::GroupHeader { group_index } = it else {
                    return false;
                };
                self.groups
                    .get(*group_index)
                    .map(|g| &g.project_id == project_id)
                    .unwrap_or(false)
            }),
            Selection::Session {
                project_id,
                session_id,
            } => self.items.iter().position(|it| {
                let Item::SessionRow {
                    group_index,
                    session_index,
                } = it
                else {
                    return false;
                };
                self.groups
                    .get(*group_index)
                    .and_then(|g| {
                        if &g.project_id == project_id {
                            g.sessions.get(*session_index)
                        } else {
                            None
                        }
                    })
                    .map(|s| &s.session_id == session_id)
                    .unwrap_or(false)
            }),
        }
    }
}

impl Default for SidebarState {
    fn default() -> Self {
        Self::new()
    }
}

/// Render the sidebar list into `area`.
///
/// The widget itself is stateless; cursor highlighting is driven through
/// ratatui's [`ListState`] derived from [`SidebarState::cursor`].
pub fn render_sidebar(frame: &mut Frame<'_>, area: Rect, state: &SidebarState, focused: bool) {
    let block_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Sessions")
        .border_style(block_style);

    let lines: Vec<ListItem> = state
        .items
        .iter()
        .map(|item| render_item(state, item))
        .collect();

    let mut list_state = ListState::default();
    list_state.select(state.cursor);
    // Seed the list state with the previous render's scroll position so
    // ratatui's auto-scroll picks up where we left off when the cursor
    // hasn't moved.
    *list_state.offset_mut() = state.scroll_offset.get();

    let highlight_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let list = List::new(lines)
        .block(block)
        .highlight_style(highlight_style)
        .highlight_symbol("▌ ");

    frame.render_stateful_widget(list, area, &mut list_state);
    // Mirror the post-render offset so the mouse hit-tester can compensate
    // for scrolling on the very next event without an extra render pass.
    state.scroll_offset.set(list_state.offset());
}

/// Render the Backends panel (`WEK-29` / M2.6 grey-state surface).
///
/// One line per known backend. Available backends render with the
/// adapter glyph in green plus their parsed version; everything else
/// renders dim with the failure reason and an optional docs URL the
/// user can copy out. The panel never gains focus — it is informational
/// only. `n` (new session) inside the sidebar already short-circuits if
/// the chosen backend is unavailable (the dispatcher refuses the
/// `sessions.create` with the right business code).
pub fn render_backends(frame: &mut Frame<'_>, area: Rect, badges: &[BackendBadge]) {
    render_backends_with_style(frame, area, badges, false)
}

/// WEK-42 / M4.3: compact-mode entry point.
///
/// Architecture §11.1 acceptance for this issue: "侧栏单色后端徽标"
/// (sidebar single-colour backend badge). When `compact` is true we
/// collapse the per-state colour code into one muted glyph and drop the
/// reason / docs sub-lines so a fleet of 10 backends fits in the same
/// space as 2-3 verbose ones would in the default layout.
pub fn render_backends_with_style(
    frame: &mut Frame<'_>,
    area: Rect,
    badges: &[BackendBadge],
    compact: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Backends")
        .border_style(Style::default().fg(Color::DarkGray));
    if badges.is_empty() {
        let body = Paragraph::new(Line::from(Span::styled(
            "no probe yet — waiting for daemon.health",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        )))
        .block(block);
        frame.render_widget(body, area);
        return;
    }

    let lines: Vec<ListItem> = if compact {
        badges
            .iter()
            .map(|b| ListItem::new(backend_compact_line(b)))
            .collect()
    } else {
        badges
            .iter()
            .flat_map(|b| backend_lines(b))
            .map(ListItem::new)
            .collect()
    };
    let list = List::new(lines).block(block);
    frame.render_widget(list, area);
}

/// Single-row, monochrome badge — `■ name (status)` or `■ name v1.2.3`.
/// Status nuance is preserved in the status-label suffix; only the
/// glyph colour collapses to one neutral tone.
fn backend_compact_line(b: &BackendBadge) -> Line<'static> {
    let glyph_color = Color::Gray;
    let suffix = if b.status == BackendHealthStatus::Available {
        match &b.version {
            Some(v) => format!("  v{v}"),
            None => String::new(),
        }
    } else {
        format!("  ({})", b.status_label())
    };
    Line::from(vec![
        Span::styled(b.glyph().to_string(), Style::default().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(
            b.display_name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            suffix,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
    ])
}

/// One or two `Line`s per backend: the primary status row + a wrapped
/// hint row for non-Available states. Returned as a `Vec` so the caller
/// can chain everything into a single `List` without juggling indices.
fn backend_lines(b: &BackendBadge) -> Vec<Line<'static>> {
    let (glyph_color, name_style) = match b.status {
        BackendHealthStatus::Available => (
            Color::Green,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        BackendHealthStatus::NotInstalled => (
            Color::DarkGray,
            // Grey + DIM is the canonical "灰态" rendering the PRD asks
            // for (PRD §5.3 + WEK-29 acceptance "未安装/未鉴权后端在侧栏
            // 显示灰态"). We intentionally do not mark these rows with
            // `Modifier::CROSSED_OUT` — that confused early reviewers
            // into thinking the backend had been removed by the user.
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::DIM | Modifier::BOLD),
        ),
        BackendHealthStatus::Unauthenticated => (
            Color::Yellow,
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::DIM | Modifier::BOLD),
        ),
        BackendHealthStatus::ProtocolDrift => (
            Color::Red,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        BackendHealthStatus::Error => (
            Color::Red,
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::DIM | Modifier::BOLD),
        ),
    };

    let suffix: String = if b.status == BackendHealthStatus::Available {
        match &b.version {
            Some(v) => format!("  v{v}"),
            None => String::new(),
        }
    } else {
        format!("  ({})", b.status_label())
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(b.glyph().to_string(), Style::default().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(b.display_name.clone(), name_style),
        Span::styled(
            suffix,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
    ])];

    if let Some(reason) = b.reason.as_deref().filter(|_| b.is_unavailable()) {
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                truncate(reason, 28),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            ),
        ]));
    }
    if let Some(url) = b.docs_url.as_deref().filter(|_| b.is_unavailable()) {
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                truncate(url, 28),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]));
    }
    lines
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn render_item<'a>(state: &'a SidebarState, item: &'a Item) -> ListItem<'a> {
    match item {
        Item::GroupHeader { group_index } => {
            let g = &state.groups[*group_index];
            let arrow = if g.expanded { "▾" } else { "▸" };
            let count = format!(" ({})", g.sessions.len());
            let title_style = if g.is_archived {
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::DIM | Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            ListItem::new(Line::from(vec![
                Span::raw(arrow),
                Span::raw(" "),
                Span::styled(g.display_name.clone(), title_style),
                Span::styled(count, Style::default().fg(Color::DarkGray)),
            ]))
        }
        Item::SessionRow {
            group_index,
            session_index,
        } => {
            let row = &state.groups[*group_index].sessions[*session_index];
            let glyph_color = match row.run_state {
                RunState::Running => Color::Green,
                RunState::Idle => Color::DarkGray,
                RunState::Waiting => Color::Yellow,
                RunState::Errored => Color::Red,
                RunState::Exited => Color::DarkGray,
            };
            ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(row.run_state.glyph(), Style::default().fg(glyph_color)),
                Span::raw(" "),
                Span::styled(row.backend.label(), Style::default().fg(Color::Magenta)),
                Span::raw("  "),
                Span::raw(row.display_title()),
            ]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Backend, ProjectGroup, RunState, SessionRow};

    fn fixture() -> Vec<ProjectGroup> {
        let row = |sid: &str, pid: &str, run: RunState| SessionRow {
            session_id: sid.to_string(),
            project_id: pid.to_string(),
            backend: Backend::new("claude"),
            title: None,
            run_state: run,
            archived: false,
            discovered: false,
        };
        let mut a = ProjectGroup::new("p-a", "proj-a");
        a.sessions.extend([
            row("s1", "p-a", RunState::Running),
            row("s2", "p-a", RunState::Idle),
        ]);
        let mut b = ProjectGroup::new("p-b", "proj-b");
        b.sessions.push(row("s3", "p-b", RunState::Running));
        let mut archived = ProjectGroup::archived();
        let mut arch_row = row("s9", "p-a", RunState::Exited);
        arch_row.archived = true;
        archived.sessions.push(arch_row);
        vec![a, b, archived]
    }

    #[test]
    fn initial_cursor_is_first_item() {
        let mut s = SidebarState::new();
        s.set_groups(fixture());
        // 2 group headers expanded + 3 sessions + archived header (folded)
        // = 1 + 2 + 1 + 1 + 1 = 6 items.
        assert_eq!(s.visible_len(), 6);
        assert!(matches!(s.selection(), Selection::Group { .. }));
    }

    #[test]
    fn jk_walks_through_visible_items() {
        let mut s = SidebarState::new();
        s.set_groups(fixture());
        // j four times: header(p-a) → s1 → s2 → header(p-b) → s3
        let after = (0..4).fold(s.selection(), |_, _| s.move_down());
        assert!(matches!(after, Selection::Session { ref session_id, .. } if session_id == "s3"));
        // One `k` walks back to the p-b group header.
        let sel = s.move_up();
        assert!(matches!(sel, Selection::Group { ref project_id } if project_id == "p-b"));
    }

    #[test]
    fn h_collapses_parent_group_and_keeps_cursor_on_header() {
        let mut s = SidebarState::new();
        s.set_groups(fixture());
        s.move_down(); // s1
        let sel = s.collapse();
        assert!(matches!(sel, Selection::Group { ref project_id } if project_id == "p-a"));
        // p-a is now folded → only 4 items.
        assert_eq!(s.visible_len(), 4);
    }

    #[test]
    fn l_expands_archived_bucket() {
        let mut s = SidebarState::new();
        s.set_groups(fixture());
        s.move_bottom(); // Archived header (folded)
        assert!(
            matches!(s.selection(), Selection::Group { ref project_id } if project_id == ProjectGroup::ARCHIVED_ID)
        );
        let sel = s.expand();
        // Still on the header (PRD: bucket expands, cursor stays).
        assert!(
            matches!(sel, Selection::Group { ref project_id } if project_id == ProjectGroup::ARCHIVED_ID)
        );
        // s9 is now visible.
        assert_eq!(s.visible_len(), 7);
    }

    #[test]
    #[allow(non_snake_case)]
    fn gG_jump_to_first_and_last() {
        let mut s = SidebarState::new();
        s.set_groups(fixture());
        s.move_down();
        s.move_down();
        let top = s.move_top();
        assert!(matches!(top, Selection::Group { ref project_id } if project_id == "p-a"));
        let bottom = s.move_bottom();
        assert!(
            matches!(bottom, Selection::Group { ref project_id } if project_id == ProjectGroup::ARCHIVED_ID)
        );
    }

    #[test]
    fn refresh_preserves_selection_and_expanded_flags() {
        let mut s = SidebarState::new();
        s.set_groups(fixture());
        s.move_down(); // s1
        s.move_bottom(); // archived header
        s.expand(); // bucket open
        let sel_before = s.selection();
        // Re-snapshot with new data (no structural change).
        s.set_groups(fixture());
        assert_eq!(s.selection(), sel_before);
        // Archived bucket is still expanded — refresh did not reset it.
        assert!(s.groups().last().unwrap().expanded);
    }

    #[test]
    fn select_index_routes_mouse_click() {
        let mut s = SidebarState::new();
        s.set_groups(fixture());
        let sel = s.select_index(1); // s1
        assert!(matches!(sel, Selection::Session { ref session_id, .. } if session_id == "s1"));
        // Out of range is ignored.
        let before = s.selection();
        s.select_index(9999);
        assert_eq!(s.selection(), before);
    }

    #[test]
    fn empty_workspace_yields_empty_selection() {
        let s = SidebarState::new();
        assert_eq!(s.selection(), Selection::Empty);
    }
}
