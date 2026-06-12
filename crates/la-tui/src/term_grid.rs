//! Full VT100 grid emulator for the session-attach pane.
//!
//! ## Why a real emulator (not a line-log)
//!
//! The agents LazyAgents drives — Claude Code, OpenAI Codex, sst.dev
//! OpenCode — are *full-screen* TUIs. They address a fixed `rows × cols`
//! cell grid with cursor motion, clear-screen, alternate-screen, and
//! scroll-region escapes, then repaint in place. A line-oriented log model
//! (append / `\n` commits / `\r` overwrites) silently drops every cursor
//! and screen-control sequence, so the rendered output is garbled.
//!
//! [`TermGrid`] therefore wraps a [`vt100::Parser`] — a true terminal
//! emulator — and [`TermGridView`] paints its [`vt100::Screen`] cell-for-
//! cell into a ratatui [`Buffer`], including the cursor and SGR styling.
//! The local grid size is kept in lock-step with the remote PTY via
//! `sessions.resize` (see [`crate::attach_pump`] / the runner), so what the
//! user sees is byte-faithful to what the agent drew.
//!
//! ## Scrollback
//!
//! `vt100` keeps its own scrollback ring. [`TermGrid::scroll`] moves the
//! parser's scrollback offset (rows from the live bottom); the live screen
//! is offset 0. A non-zero offset is surfaced by the runner as a "more
//! below" affordance, mirroring the old transcript behaviour.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Widget},
};

/// Default scrollback depth (rows retained above the live screen). Matches
/// the PRD §5.3 "long-output scrolling" bar that the previous line-log
/// model capped at 10_000.
const DEFAULT_SCROLLBACK: usize = 10_000;

/// One user scroll input the surrounding widget translates from a key
/// event, applied by [`TermGrid::scroll`] against the viewport height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    LineUp,
    LineDown,
    /// Jump to the oldest retained row.
    Top,
    /// Jump back to the live screen (offset 0).
    Bottom,
}

/// A live VT100 grid fed by raw PTY bytes.
pub struct TermGrid {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    /// Current scrollback offset in rows above the live screen. 0 == live.
    scrollback: usize,
}

impl Default for TermGrid {
    fn default() -> Self {
        // 24×80 mirrors the daemon's initial PtySize; the runner resizes to
        // the real pane on first render.
        Self::new(24, 80)
    }
}

impl TermGrid {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            parser: vt100::Parser::new(rows, cols, DEFAULT_SCROLLBACK),
            rows,
            cols,
            scrollback: 0,
        }
    }

    /// Feed a chunk of PTY bytes. Safe with partial escape sequences —
    /// the parser is byte-incremental.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the local grid to match the attach pane. Should be paired
    /// with a `sessions.resize` so the remote PTY reflows to the same
    /// dimensions. No-op when unchanged.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(rows, cols);
        self.clamp_scrollback();
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn scrollback(&self) -> usize {
        self.scrollback
    }

    /// True when the view is glued to the live screen (offset 0).
    pub fn is_following(&self) -> bool {
        self.scrollback == 0
    }

    /// Apply a scroll input against a viewport of `viewport_height` rows.
    pub fn scroll(&mut self, action: ScrollAction, viewport_height: usize) {
        let h = viewport_height.max(1);
        let half = (h / 2).max(1);
        // Larger offset == further back in history.
        let next = match action {
            ScrollAction::HalfPageUp => self.scrollback.saturating_add(half),
            ScrollAction::HalfPageDown => self.scrollback.saturating_sub(half),
            ScrollAction::PageUp => self.scrollback.saturating_add(h),
            ScrollAction::PageDown => self.scrollback.saturating_sub(h),
            ScrollAction::LineUp => self.scrollback.saturating_add(1),
            ScrollAction::LineDown => self.scrollback.saturating_sub(1),
            ScrollAction::Top => DEFAULT_SCROLLBACK,
            ScrollAction::Bottom => 0,
        };
        self.scrollback = next;
        self.clamp_scrollback();
        self.parser.set_scrollback(self.scrollback);
    }

    fn clamp_scrollback(&mut self) {
        if self.scrollback > DEFAULT_SCROLLBACK {
            self.scrollback = DEFAULT_SCROLLBACK;
        }
        self.parser.set_scrollback(self.scrollback);
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

/// Map a `vt100` color to a ratatui [`Color`]. `Default` leaves the cell
/// unset so the surrounding pane style shows through.
fn map_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Translate one emulator cell's attributes into a ratatui [`Style`].
fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(fg) = map_color(cell.fgcolor()) {
        style = style.fg(fg);
    }
    if let Some(bg) = map_color(cell.bgcolor()) {
        style = style.bg(bg);
    }
    let mut mods = Modifier::empty();
    if cell.bold() {
        mods |= Modifier::BOLD;
    }
    if cell.italic() {
        mods |= Modifier::ITALIC;
    }
    if cell.underline() {
        mods |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        mods |= Modifier::REVERSED;
    }
    style.add_modifier(mods)
}

/// Stateless ratatui widget that paints a [`TermGrid`]'s screen into
/// `area`, cell-for-cell, plus the cursor.
///
/// Borrows the grid immutably — rendering never mutates emulator state.
/// Optionally wraps the grid in a `Block` for the surrounding chrome.
pub struct TermGridView<'a> {
    grid: &'a TermGrid,
    block: Option<Block<'a>>,
    base_style: Style,
}

impl<'a> TermGridView<'a> {
    pub fn new(grid: &'a TermGrid) -> Self {
        Self {
            grid,
            block: None,
            base_style: Style::default(),
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.base_style = style;
        self
    }
}

impl<'a> Widget for TermGridView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let inner = if let Some(block) = &self.block {
            let inner = block.inner(area);
            block.clone().render(area, buf);
            inner
        } else {
            area
        };
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let screen = self.grid.screen();
        let (grid_rows, grid_cols) = screen.size();
        let max_rows = grid_rows.min(inner.height);
        let max_cols = grid_cols.min(inner.width);

        for row in 0..max_rows {
            for col in 0..max_cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                // Wide-char continuation cells carry no glyph; the lead
                // cell already drew the 2-wide character.
                if cell.is_wide_continuation() {
                    continue;
                }
                let x = inner.x + col;
                let y = inner.y + row;
                let target = &mut buf[(x, y)];
                let contents = cell.contents();
                if contents.is_empty() {
                    target.set_char(' ');
                } else {
                    target.set_symbol(&contents);
                }
                target.set_style(self.base_style.patch(cell_style(cell)));
            }
        }

        // Cursor: paint as a reversed cell on the live screen only. When
        // scrolled back into history the cursor would be misleading, and
        // when the agent hid it (`civis`) we honour that.
        if self.grid.is_following() && !screen.hide_cursor() {
            let (cy, cx) = screen.cursor_position();
            if cy < max_rows && cx < max_cols {
                let x = inner.x + cx;
                let y = inner.y + cy;
                let target = &mut buf[(x, y)];
                target.set_style(
                    target
                        .style()
                        .patch(Style::default().add_modifier(Modifier::REVERSED)),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn render(grid: &TermGrid, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        TermGridView::new(grid).render(area, &mut buf);
        buf
    }

    fn row_string(buf: &Buffer, y: u16, w: u16) -> String {
        (0..w)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn plain_text_lands_on_the_grid() {
        let mut g = TermGrid::new(4, 20);
        g.feed(b"hello");
        let buf = render(&g, 20, 4);
        assert!(row_string(&buf, 0, 20).starts_with("hello"));
    }

    #[test]
    fn cursor_addressing_places_text_absolutely() {
        // CUP to row 3 col 10 (1-based in the escape), then write.
        let mut g = TermGrid::new(6, 30);
        g.feed(b"\x1b[3;10HX");
        let buf = render(&g, 30, 6);
        // Row index 2 (0-based), column 9 carries the 'X'.
        assert_eq!(buf[(9u16, 2u16)].symbol(), "X");
    }

    #[test]
    fn clear_screen_wipes_prior_output() {
        let mut g = TermGrid::new(4, 20);
        g.feed(b"garbage everywhere");
        g.feed(b"\x1b[2J\x1b[H");
        g.feed(b"clean");
        let buf = render(&g, 20, 4);
        let row0 = row_string(&buf, 0, 20);
        assert!(row0.starts_with("clean"), "row0 was {row0:?}");
        assert!(!row0.contains("garbage"), "stale content survived clear");
    }

    #[test]
    fn alternate_screen_enter_and_leave_restores_primary() {
        let mut g = TermGrid::new(4, 20);
        g.feed(b"primary");
        // Enter alternate screen, draw, then leave.
        g.feed(b"\x1b[?1049h");
        g.feed(b"\x1b[2J\x1b[Halt");
        let alt = render(&g, 20, 4);
        assert!(row_string(&alt, 0, 20).starts_with("alt"));
        g.feed(b"\x1b[?1049l");
        let primary = render(&g, 20, 4);
        assert!(
            row_string(&primary, 0, 20).starts_with("primary"),
            "leaving alt screen should restore primary"
        );
    }

    #[test]
    fn sgr_color_maps_to_cell_style() {
        let mut g = TermGrid::new(2, 10);
        g.feed(b"\x1b[31mR\x1b[0m");
        let buf = render(&g, 10, 2);
        assert_eq!(buf[(0u16, 0u16)].fg, Color::Indexed(1));
    }

    #[test]
    fn carriage_return_overwrite_is_grid_correct() {
        // Progress-bar idiom: write, CR, overwrite from column 0.
        let mut g = TermGrid::new(2, 20);
        g.feed(b"loading\rDONE");
        let buf = render(&g, 20, 2);
        assert!(row_string(&buf, 0, 20).starts_with("DONEing"));
    }

    #[test]
    fn resize_reflows_without_panic() {
        let mut g = TermGrid::new(10, 80);
        g.feed(b"hello world");
        g.resize(24, 100);
        assert_eq!(g.size(), (24, 100));
        let buf = render(&g, 100, 24);
        assert!(row_string(&buf, 0, 100).starts_with("hello world"));
    }

    #[test]
    fn scroll_offset_tracks_follow_state() {
        let mut g = TermGrid::new(4, 20);
        for i in 0..50 {
            g.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(g.is_following());
        g.scroll(ScrollAction::PageUp, 4);
        assert!(!g.is_following());
        assert!(g.scrollback() > 0);
        g.scroll(ScrollAction::Bottom, 4);
        assert!(g.is_following());
        assert_eq!(g.scrollback(), 0);
    }
}
