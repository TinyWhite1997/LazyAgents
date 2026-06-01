//! Bottom-of-screen prompt composer.
//!
//! [`Composer`] is a deliberately small multi-line text buffer: it has a
//! cursor, supports the editing keys a user expects from a single-pane
//! input box, and recalls past submissions with `Up`/`Down`. It does NOT
//! pretend to be a full editor (no undo, no selection, no wrap-aware
//! navigation) — for anything richer, the user drops into the external
//! editor via the `o` shortcut on the surrounding pane (PRD §5.3).
//!
//! ## Send key
//!
//! The task spec calls out `Ctrl+Enter` as the send key. Plain `Enter`
//! inserts a literal newline so multi-paragraph prompts are natural to
//! type. That mirrors what users already know from Slack / Discord /
//! ChatGPT and avoids the "I lost my message because I pressed Enter
//! mid-thought" failure mode.
//!
//! ## History
//!
//! A fixed-size ring of past submissions is searchable with `Up`/`Down`:
//! `Up` walks toward older entries, `Down` toward newer. The first time
//! the user starts walking, the in-flight draft is stashed so they can
//! `Down` back into it without losing what they had typed. Sending a
//! prompt resets the history cursor.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget},
};

/// Outcome of a state-changing call on [`Composer`] (currently only
/// [`Composer::history_prev`] and [`Composer::history_next`] return one).
///
/// The composer deliberately does NOT own a key router: the surrounding
/// app translates raw key events into composer method calls so that key
/// bindings, modal state, and the global key-hints overlay stay in one
/// place. `ComposerAction` exists so the host can know when a method
/// changed visible state without redundantly diffing the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    /// Nothing observable changed (e.g. `history_prev` with an empty
    /// history).
    Idle,
    /// The buffer or cursor changed; repaint the composer.
    Repaint,
    /// Reserved for a future `on_key`-style helper that batches submit;
    /// not currently produced by any method here but kept so the variant
    /// name reads naturally from a host's match arm.
    Send { text: String },
}

const DEFAULT_HISTORY: usize = 200;

pub struct Composer {
    lines: Vec<String>,
    /// Cursor position. `row` indexes into `lines`; `col` is a byte
    /// offset inside that line (we keep insertion ASCII-safe by jumping
    /// to char boundaries before writing).
    row: usize,
    col: usize,
    /// Most-recent-last ring of submitted prompts.
    history: Vec<String>,
    history_limit: usize,
    /// `None` when the user isn't browsing history. `Some(i)` selects
    /// `history[i]` and stashes the live draft in [`Self::draft_stash`].
    history_cursor: Option<usize>,
    draft_stash: Option<Vec<String>>,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY)
    }
}

impl Composer {
    pub fn new(history_limit: usize) -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            history: Vec::new(),
            history_limit: history_limit.max(1),
            history_cursor: None,
            draft_stash: None,
        }
    }

    /// Current text, with embedded `\n`s.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer holds no characters at all (single empty
    /// line). Used by the renderer to display the placeholder.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Clear the buffer and reset the cursor — call after a successful
    /// send so the composer is ready for the next prompt.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.history_cursor = None;
        self.draft_stash = None;
    }

    /// Replace the buffer with `text` (used to inject a history entry).
    fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_owned).collect()
        };
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    pub fn insert_char(&mut self, ch: char) {
        // Snap cursor onto a char boundary just in case caller mutated
        // history mid-edit.
        self.snap_col();
        self.lines[self.row].insert(self.col, ch);
        self.col += ch.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        let tail = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let prev = prev_char_boundary(&self.lines[self.row], self.col);
            self.lines[self.row].drain(prev..self.col);
            self.col = prev;
        } else if self.row > 0 {
            let removed = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&removed);
        }
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col = prev_char_boundary(&self.lines[self.row], self.col);
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].len();
        if self.col < len {
            self.col = next_char_boundary(&self.lines[self.row], self.col);
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_to_line_start(&mut self) {
        self.col = 0;
    }

    pub fn move_to_line_end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// History recall: walk toward older entries. Caller routes this from
    /// `Up` when (and only when) the cursor is on the first line — that's
    /// the convention Slack/Discord/ChatGPT use, so users don't get a
    /// history jump when they meant to move the caret up inside a
    /// multi-line draft.
    pub fn history_prev(&mut self) -> ComposerAction {
        if self.history.is_empty() {
            return ComposerAction::Idle;
        }
        let next = match self.history_cursor {
            None => {
                self.draft_stash = Some(self.lines.clone());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        let entry = self.history[next].clone();
        self.set_text(&entry);
        ComposerAction::Repaint
    }

    /// History recall: walk toward newer entries. Stepping past the
    /// newest entry restores the stashed draft, if any.
    pub fn history_next(&mut self) -> ComposerAction {
        let Some(i) = self.history_cursor else {
            return ComposerAction::Idle;
        };
        if i + 1 < self.history.len() {
            self.history_cursor = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.set_text(&entry);
        } else {
            self.history_cursor = None;
            let stash = self
                .draft_stash
                .take()
                .unwrap_or_else(|| vec![String::new()]);
            self.lines = stash;
            self.row = self.lines.len() - 1;
            self.col = self.lines[self.row].len();
        }
        ComposerAction::Repaint
    }

    /// Whether the cursor sits on the first visual line — used by the
    /// outer key router to decide between "move caret up" and "history
    /// previous".
    pub fn cursor_on_first_line(&self) -> bool {
        self.row == 0
    }

    pub fn cursor_on_last_line(&self) -> bool {
        self.row + 1 == self.lines.len()
    }

    /// Take the current text as a submission. Pushes onto history (dedup
    /// against the most recent entry) and clears the buffer. Returns the
    /// submitted text — including the case where the buffer was empty,
    /// since the caller may still want to short-circuit there.
    pub fn submit(&mut self) -> String {
        let text = self.text();
        if !text.is_empty() && self.history.last().map(String::as_str) != Some(text.as_str()) {
            self.history.push(text.clone());
            if self.history.len() > self.history_limit {
                let excess = self.history.len() - self.history_limit;
                self.history.drain(0..excess);
            }
        }
        self.clear();
        text
    }

    fn snap_col(&mut self) {
        let line = &self.lines[self.row];
        if self.col > line.len() {
            self.col = line.len();
        }
        while !line.is_char_boundary(self.col) && self.col > 0 {
            self.col -= 1;
        }
    }
}

fn prev_char_boundary(s: &str, mut i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    i -= 1;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, mut i: usize) -> usize {
    let len = s.len();
    if i >= len {
        return len;
    }
    i += 1;
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// ratatui widget rendering the composer with placeholder + cursor.
///
/// Surrounds in a `Block` if one is supplied. The cursor is drawn via a
/// reverse-video cell at the caret position so we don't fight crossterm's
/// real cursor (which the surrounding `App` may want to position
/// differently).
pub struct ComposerView<'a> {
    composer: &'a Composer,
    block: Option<Block<'a>>,
    placeholder: Option<&'a str>,
    style: Style,
}

impl<'a> ComposerView<'a> {
    pub fn new(composer: &'a Composer) -> Self {
        Self {
            composer,
            block: None,
            placeholder: None,
            style: Style::default(),
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn placeholder(mut self, text: &'a str) -> Self {
        self.placeholder = Some(text);
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl<'a> Widget for ComposerView<'a> {
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
        let cursor_style = Style::default().bg(Color::Gray).fg(Color::Black);
        if self.composer.is_empty() {
            if let Some(ph) = self.placeholder {
                let line = Line::from(vec![
                    Span::styled(" ", cursor_style),
                    Span::styled(ph, Style::default().fg(Color::DarkGray)),
                ]);
                Paragraph::new(line).style(self.style).render(inner, buf);
                return;
            }
        }

        let (cur_row, mut cur_col) = self.composer.cursor();
        let mut visible: Vec<Line> = Vec::with_capacity(self.composer.lines.len());
        for (row, line) in self.composer.lines.iter().enumerate() {
            if row != cur_row {
                visible.push(Line::from(line.as_str()));
                continue;
            }
            // Defensive: snap to a char boundary before splitting. The
            // editing API keeps the cursor on a boundary, but a future
            // history injection or direct mutation could leave it mid-
            // codepoint — `split_at` on a non-boundary panics, and the
            // render path is the wrong place to discover that.
            if cur_col > line.len() {
                cur_col = line.len();
            }
            while cur_col > 0 && !line.is_char_boundary(cur_col) {
                cur_col -= 1;
            }
            let (left, right) = line.split_at(cur_col);
            let (caret_ch, tail) = if right.is_empty() {
                (" ", "")
            } else {
                let mut it = right.char_indices();
                it.next();
                let rest_start = it.next().map(|(i, _)| i).unwrap_or(right.len());
                (&right[..rest_start], &right[rest_start..])
            };
            visible.push(Line::from(vec![
                Span::raw(left),
                Span::styled(caret_ch, cursor_style),
                Span::raw(tail),
            ]));
        }
        Paragraph::new(visible).style(self.style).render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_char_and_newline() {
        let mut c = Composer::new(10);
        for ch in "hello".chars() {
            c.insert_char(ch);
        }
        c.insert_newline();
        for ch in "world".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.text(), "hello\nworld");
        assert_eq!(c.cursor(), (1, 5));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut c = Composer::new(10);
        for ch in "abc".chars() {
            c.insert_char(ch);
        }
        c.insert_newline();
        for ch in "def".chars() {
            c.insert_char(ch);
        }
        // Cursor at end of "def", row 1, col 3.
        c.move_to_line_start();
        c.backspace(); // joins with previous line
        assert_eq!(c.text(), "abcdef");
        assert_eq!(c.cursor(), (0, 3));
    }

    #[test]
    fn submit_pushes_history_and_clears() {
        let mut c = Composer::new(3);
        for ch in "first".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.submit(), "first");
        assert!(c.is_empty());
        assert_eq!(c.history(), &["first".to_string()]);
    }

    #[test]
    fn history_dedupes_consecutive_duplicates() {
        let mut c = Composer::new(10);
        for ch in "msg".chars() {
            c.insert_char(ch);
        }
        c.submit();
        for ch in "msg".chars() {
            c.insert_char(ch);
        }
        c.submit();
        assert_eq!(c.history().len(), 1);
    }

    #[test]
    fn history_limit_drops_oldest() {
        let mut c = Composer::new(2);
        for s in ["a", "b", "c"] {
            for ch in s.chars() {
                c.insert_char(ch);
            }
            c.submit();
        }
        assert_eq!(c.history(), &["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn history_prev_recalls_latest() {
        let mut c = Composer::new(10);
        for s in ["one", "two", "three"] {
            for ch in s.chars() {
                c.insert_char(ch);
            }
            c.submit();
        }
        assert_eq!(c.history_prev(), ComposerAction::Repaint);
        assert_eq!(c.text(), "three");
        assert_eq!(c.history_prev(), ComposerAction::Repaint);
        assert_eq!(c.text(), "two");
        assert_eq!(c.history_prev(), ComposerAction::Repaint);
        assert_eq!(c.text(), "one");
        // Already at oldest — further `Up` should clamp, not crash.
        assert_eq!(c.history_prev(), ComposerAction::Repaint);
        assert_eq!(c.text(), "one");
    }

    #[test]
    fn history_next_restores_stashed_draft() {
        let mut c = Composer::new(10);
        for ch in "old".chars() {
            c.insert_char(ch);
        }
        c.submit();
        for ch in "draft".chars() {
            c.insert_char(ch);
        }
        // Now walk into history.
        c.history_prev();
        assert_eq!(c.text(), "old");
        // Step back out — draft should reappear.
        assert_eq!(c.history_next(), ComposerAction::Repaint);
        assert_eq!(c.text(), "draft");
        // Already past newest, further Down is a no-op.
        assert_eq!(c.history_next(), ComposerAction::Idle);
    }

    #[test]
    fn history_prev_with_empty_history_is_idle() {
        let mut c = Composer::new(10);
        assert_eq!(c.history_prev(), ComposerAction::Idle);
    }

    #[test]
    fn unicode_backspace_respects_boundaries() {
        let mut c = Composer::new(10);
        c.insert_char('我');
        c.insert_char('们');
        c.backspace();
        assert_eq!(c.text(), "我");
        c.backspace();
        assert_eq!(c.text(), "");
    }

    #[test]
    fn render_with_unicode_cursor_does_not_panic() {
        // Defensive: even if a future path leaves col mid-codepoint, the
        // render must not crash. We exercise the well-formed case (col on
        // a boundary) since the public API never leaves an invalid col,
        // and rely on the snap inside ComposerView for the bad case.
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
        let mut c = Composer::new(10);
        c.insert_char('我');
        c.insert_char('们');
        let area = Rect::new(0, 0, 8, 1);
        let mut buf = Buffer::empty(area);
        ComposerView::new(&c).render(area, &mut buf);
    }
}
