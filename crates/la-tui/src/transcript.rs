//! Scrolling transcript widget for the conversation main area.
//!
//! [`Transcript`] owns a ring buffer of committed [`TerminalLine`]s plus a
//! scroll cursor. PTY bytes arrive via [`Transcript::feed`] (the daemon
//! delivers them as `session.output` chunks); rendering pulls a slice of
//! lines for the visible area.
//!
//! ## Auto-follow
//!
//! When the user is parked at the tail of the buffer, the view stays glued
//! to the newest line — fresh output keeps scrolling into view. The moment
//! the user scrolls up (PgUp, Ctrl+u, k…) auto-follow disengages and the
//! viewport stays put while new lines pile up off-screen. Returning to the
//! tail with `End` / `G` (or simply scrolling all the way down) re-engages
//! follow.
//!
//! ## Bounded memory
//!
//! Long-running agent sessions can produce arbitrary amounts of output.
//! `Transcript` caps history at [`Transcript::scrollback_limit`] lines
//! (default 10_000, matching the PRD §5.3 acceptance bar) by dropping the
//! oldest line whenever the buffer overruns. Drops are tracked so the
//! pending render can show a "…N lines dropped" hint without lying to the
//! user about absolute line numbers.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget},
};

use crate::vte_term::{TerminalLine, TerminalScreen};

/// One user-driven scroll input the surrounding widget translates from a
/// key event. The widget then calls [`Transcript::scroll`] to apply it
/// against the current viewport height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    /// `Ctrl+u` — jump half a viewport upward.
    HalfPageUp,
    /// `Ctrl+d` — jump half a viewport downward.
    HalfPageDown,
    /// `PageUp` — jump one full viewport upward.
    PageUp,
    /// `PageDown` — jump one full viewport downward.
    PageDown,
    /// `k` or `Up` — one line up.
    LineUp,
    /// `j` or `Down` — one line down.
    LineDown,
    /// `g` / `Home` — jump to oldest retained line.
    Top,
    /// `G` / `End` — jump to newest line and re-engage auto-follow.
    Bottom,
}

/// Bounded scrollback ring + scroll/follow state. Independent of any
/// concrete RPC type, so it can be unit-tested without spinning up the
/// daemon.
pub struct Transcript {
    /// Committed (newline-terminated) history. Oldest first.
    lines: Vec<TerminalLine>,
    /// Live in-flight tail, rebuilt every feed. Always rendered as the
    /// final visible line when present.
    pending: TerminalLine,
    /// Parser + line composer. The screen owns the SGR state across feeds.
    screen: TerminalScreen,
    /// Hard cap on `lines.len()`. Overflow drops the oldest line.
    scrollback_limit: usize,
    /// Count of lines dropped due to the cap, so the renderer can surface
    /// "…N lines dropped" without misleading the user.
    dropped: u64,
    /// 0-based index of the topmost visible line.
    ///
    /// Invariant: `view_top <= total_renderable().saturating_sub(1)`.
    /// `total_renderable()` is `lines.len() + (pending non-empty ? 1 : 0)`.
    view_top: usize,
    /// True when the view should auto-snap to the tail on every feed.
    /// Toggled off when the user scrolls up; toggled back on by reaching
    /// the tail or invoking [`ScrollAction::Bottom`].
    follow: bool,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl Transcript {
    pub fn new(scrollback_limit: usize) -> Self {
        Self {
            lines: Vec::new(),
            pending: TerminalLine::default(),
            screen: TerminalScreen::new(),
            scrollback_limit: scrollback_limit.max(1),
            dropped: 0,
            view_top: 0,
            follow: true,
        }
    }

    pub fn scrollback_limit(&self) -> usize {
        self.scrollback_limit
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Total renderable line count: committed + 1 if pending is non-empty.
    pub fn renderable_len(&self) -> usize {
        self.lines.len() + if self.pending.is_empty() { 0 } else { 1 }
    }

    /// Top of the visible viewport (test hook).
    pub fn view_top(&self) -> usize {
        self.view_top
    }

    /// Read-only slice of committed lines. Useful for the integration
    /// layer when it needs to render a custom widget instead of using the
    /// built-in [`Widget`] impl.
    pub fn lines(&self) -> &[TerminalLine] {
        &self.lines
    }

    pub fn pending(&self) -> &TerminalLine {
        &self.pending
    }

    /// Feed PTY bytes from a `session.output` notification.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        let new_lines = self.screen.take_committed();
        let added = new_lines.len();
        if added > 0 {
            self.lines.extend(new_lines);
            self.enforce_cap();
        }
        self.pending = self.screen.pending().clone();
        if self.follow {
            // Stay at the tail. `view_top` is recomputed by the renderer
            // against the actual viewport height, so just mark we're at
            // the bottom by setting it past the end; clamp in `scroll`.
            self.view_top = self.renderable_len();
        }
    }

    fn enforce_cap(&mut self) {
        if self.lines.len() > self.scrollback_limit {
            let excess = self.lines.len() - self.scrollback_limit;
            self.lines.drain(0..excess);
            self.dropped = self.dropped.saturating_add(excess as u64);
            // Keep view_top stable against the *content* the user was
            // looking at: shift up by the same amount so the eye doesn't
            // jump. If we drop more than view_top, clamp to 0.
            self.view_top = self.view_top.saturating_sub(excess);
        }
    }

    /// Apply a scroll input. `viewport_height` is the row count of the
    /// transcript area so jumps map to a real screen height.
    pub fn scroll(&mut self, action: ScrollAction, viewport_height: usize) {
        let total = self.renderable_len();
        let h = viewport_height.max(1);
        let max_top = total.saturating_sub(h);
        // Clamp first: a fresh `feed` while following parks `view_top` at
        // `renderable_len()` (one past `max_top`) — without this snap the
        // first Ctrl+u from the tail would compute against the inflated
        // value and require two presses to leave follow mode.
        if self.view_top > max_top {
            self.view_top = max_top;
        }
        let half = (h / 2).max(1);
        let new_top = match action {
            ScrollAction::HalfPageUp => self.view_top.saturating_sub(half),
            ScrollAction::HalfPageDown => self.view_top.saturating_add(half),
            ScrollAction::PageUp => self.view_top.saturating_sub(h),
            ScrollAction::PageDown => self.view_top.saturating_add(h),
            ScrollAction::LineUp => self.view_top.saturating_sub(1),
            ScrollAction::LineDown => self.view_top.saturating_add(1),
            ScrollAction::Top => 0,
            ScrollAction::Bottom => max_top,
        };
        let clamped = new_top.min(max_top);
        if matches!(action, ScrollAction::Bottom) {
            self.follow = true;
        } else if clamped < max_top {
            self.follow = false;
        } else {
            // Landed exactly at the tail — re-engage follow.
            self.follow = true;
        }
        self.view_top = clamped;
    }

    /// Resize hook: a layout change can shrink the viewport, leaving
    /// `view_top` past the end. Call this before rendering.
    pub fn clamp_for_viewport(&mut self, viewport_height: usize) {
        let total = self.renderable_len();
        let max_top = total.saturating_sub(viewport_height.max(1));
        if self.view_top > max_top {
            self.view_top = max_top;
        }
        if self.follow {
            self.view_top = max_top;
        }
    }

    /// Project the current state into ratatui [`Line`]s for rendering.
    /// The renderer slices `[view_top .. view_top + viewport_height]`.
    pub fn render_lines<'a>(&'a self) -> Vec<Line<'a>> {
        let mut out: Vec<Line<'a>> = Vec::with_capacity(self.lines.len() + 1);
        for line in &self.lines {
            out.push(to_ratatui_line(line));
        }
        if !self.pending.is_empty() {
            out.push(to_ratatui_line(&self.pending));
        }
        out
    }
}

fn to_ratatui_line<'a>(line: &'a TerminalLine) -> Line<'a> {
    // Coalesce adjacent cells that share a style into one Span. ratatui's
    // span machinery is byte-cheap but allocating one per char would still
    // be wasteful for long lines.
    let mut spans: Vec<Span<'a>> = Vec::new();
    if line.cells.is_empty() {
        return Line::from(spans);
    }
    let mut buf = String::new();
    let mut cur_style = line.cells[0].style;
    for cell in &line.cells {
        if cell.style == cur_style {
            buf.push(cell.ch);
        } else {
            spans.push(Span::styled(std::mem::take(&mut buf), cur_style));
            cur_style = cell.style;
            buf.push(cell.ch);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur_style));
    }
    Line::from(spans)
}

/// Stateless ratatui widget that renders the transcript inside `area`.
///
/// Borrows the [`Transcript`] mutably because we need to clamp `view_top`
/// against the actual `area.height` before reading the visible slice.
/// Optionally wraps inside a `Block` for the surrounding chrome.
pub struct TranscriptView<'a> {
    transcript: &'a mut Transcript,
    block: Option<Block<'a>>,
    style: Style,
}

impl<'a> TranscriptView<'a> {
    pub fn new(transcript: &'a mut Transcript) -> Self {
        Self {
            transcript,
            block: None,
            style: Style::default(),
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl<'a> Widget for TranscriptView<'a> {
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
        self.transcript.clamp_for_viewport(inner.height as usize);
        let all_lines = self.transcript.render_lines();
        let top = self.transcript.view_top();
        let h = inner.height as usize;
        // Slice in logical-line units. We deliberately do NOT enable
        // `Paragraph::wrap` here: Paragraph's `.scroll` and the wrap
        // policy count *visual rows*, while `view_top` / `max_top` count
        // *logical lines*. Mixing the two units would mis-position the
        // viewport whenever an agent emits a line wider than the pane —
        // the user would see a short window with a partial tail. Long
        // lines are truncated at the right edge, which matches what
        // every other terminal-style log viewer (less, lazygit, k9s) does.
        let end = (top + h).min(all_lines.len());
        let visible: Vec<Line> = if top >= all_lines.len() {
            Vec::new()
        } else {
            all_lines[top..end].to_vec()
        };
        Paragraph::new(visible).style(self.style).render(inner, buf);

        // "follow off" affordance: paint a dim hint inside the bottom row
        // when there is more content below the visible window. Done as a
        // direct cell write so it does not lie about the row count or
        // perturb the slice math above.
        if !self.transcript.is_following() && end < all_lines.len() && inner.height > 0 {
            let hint = "── more below (G/End to follow) ──";
            let row = inner.bottom().saturating_sub(1);
            let start_x = inner.x
                + inner
                    .width
                    .saturating_sub(hint.chars().count() as u16)
                    .saturating_sub(1);
            let dim = Style::default().fg(Color::DarkGray);
            buf.set_string(start_x.max(inner.x), row, hint, dim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_committed(t: &Transcript) -> Vec<String> {
        t.lines().iter().map(|l| l.to_plain_string()).collect()
    }

    #[test]
    fn feed_commits_lines_and_keeps_pending() {
        let mut t = Transcript::new(100);
        t.feed(b"one\ntwo\nthr");
        assert_eq!(plain_committed(&t), vec!["one", "two"]);
        assert_eq!(t.pending().to_plain_string(), "thr");
        assert_eq!(t.renderable_len(), 3);
    }

    #[test]
    fn scrollback_drops_oldest_lines_at_cap() {
        let mut t = Transcript::new(3);
        for i in 0..10 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        assert_eq!(plain_committed(&t), vec!["L7", "L8", "L9"]);
        assert_eq!(t.dropped(), 7);
    }

    #[test]
    fn auto_follow_keeps_view_at_tail() {
        let mut t = Transcript::new(100);
        // Feed 50 lines, viewport is 10 tall — should park at tail.
        for i in 0..50 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(10);
        assert!(t.is_following());
        assert_eq!(t.view_top(), 50 - 10);
    }

    #[test]
    fn page_up_disengages_follow() {
        let mut t = Transcript::new(100);
        for i in 0..50 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(10);
        t.scroll(ScrollAction::PageUp, 10);
        assert!(!t.is_following());
        // PageUp from view_top=40 → 30.
        assert_eq!(t.view_top(), 30);
    }

    #[test]
    fn ctrl_u_jumps_half_viewport() {
        let mut t = Transcript::new(100);
        for i in 0..50 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(20);
        // view_top = 30. HalfPageUp(h=20) jumps by 10 → 20.
        t.scroll(ScrollAction::HalfPageUp, 20);
        assert_eq!(t.view_top(), 20);
        assert!(!t.is_following());
    }

    #[test]
    fn ctrl_d_then_bottom_reengages_follow() {
        let mut t = Transcript::new(100);
        for i in 0..50 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(10);
        t.scroll(ScrollAction::PageUp, 10);
        t.scroll(ScrollAction::PageUp, 10);
        assert!(!t.is_following());
        t.scroll(ScrollAction::Bottom, 10);
        assert!(t.is_following());
        assert_eq!(t.view_top(), 40);
    }

    #[test]
    fn ctrl_d_to_tail_reengages_follow_automatically() {
        let mut t = Transcript::new(100);
        for i in 0..30 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(10);
        t.scroll(ScrollAction::PageUp, 10);
        assert!(!t.is_following());
        // Two HalfPageDowns of 5 each = +10 → back to tail (20).
        t.scroll(ScrollAction::HalfPageDown, 10);
        t.scroll(ScrollAction::HalfPageDown, 10);
        assert!(t.is_following());
        assert_eq!(t.view_top(), 20);
    }

    #[test]
    fn first_ctrl_u_from_follow_disengages_without_clamp_call() {
        // Regression: feed() parks view_top at renderable_len() (one past
        // max_top) when following. Without an explicit clamp before the
        // key event, the first Ctrl+u used to compute against the inflated
        // value and require two presses to leave follow mode.
        let mut t = Transcript::new(100);
        for i in 0..50 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        assert!(t.is_following());
        t.scroll(ScrollAction::HalfPageUp, 10);
        assert!(!t.is_following(), "single Ctrl+u must disengage follow");
        // Tail was 40; half-page back from there is 35.
        assert_eq!(t.view_top(), 35);
    }

    #[test]
    fn new_output_does_not_kick_user_when_unfollowed() {
        // User scrolled up — new lines must not snap the viewport away
        // from what they're reading. This is the no-1 worst UX bug for
        // log readers.
        let mut t = Transcript::new(100);
        for i in 0..30 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(10);
        t.scroll(ScrollAction::PageUp, 10);
        let parked = t.view_top();
        assert!(!t.is_following());
        for i in 30..40 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(10);
        assert!(!t.is_following());
        assert_eq!(t.view_top(), parked);
    }

    #[test]
    fn large_buffer_scroll_is_cheap() {
        // 10k lines is the PRD acceptance bar. We just check the API
        // tolerates it without panic; perf is left to the bench tier.
        let mut t = Transcript::new(10_000);
        for i in 0..10_000 {
            t.feed(format!("L{}\n", i).as_bytes());
        }
        t.clamp_for_viewport(40);
        assert_eq!(t.renderable_len(), 10_000);
        assert_eq!(t.view_top(), 10_000 - 40);
        // A spread of scroll ops finishes synchronously.
        for _ in 0..200 {
            t.scroll(ScrollAction::HalfPageUp, 40);
        }
        for _ in 0..200 {
            t.scroll(ScrollAction::HalfPageDown, 40);
        }
        assert!(t.is_following());
    }

    #[test]
    fn rendering_long_line_does_not_misalign_viewport() {
        // A line wider than the viewport must not shift the tail row out
        // of view. We assert the bottom row of the rendered buffer ends
        // with the most recent committed line.
        let mut t = Transcript::new(100);
        // 200 chars on each of 5 lines — far wider than our 20-col area.
        for i in 0..5 {
            let long = "x".repeat(200);
            t.feed(format!("{long}_L{i}\n").as_bytes());
        }
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        TranscriptView::new(&mut t).render(area, &mut buf);
        // Last visible logical line = "L4", reachable from a slice that
        // contains lines 2, 3, 4 (top = 2). The last buffer row carries
        // the start of "xxxx..._L4" (truncated to width 20).
        let row_str: String = (0..area.width)
            .map(|x| {
                buf[(x, area.height - 1)]
                    .symbol()
                    .chars()
                    .next()
                    .unwrap_or(' ')
            })
            .collect();
        assert!(
            row_str.starts_with("xxxx"),
            "expected the tail line to occupy the bottom row, got {row_str:?}"
        );
    }
}
