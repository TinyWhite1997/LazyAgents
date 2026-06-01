//! Transient "会话仍在后台运行" toast surfaced after `sessions.detach`.
//!
//! When the user detaches from a live agent, the obvious worry is "did
//! I just kill the agent?". The detach notice answers that explicitly,
//! disappearing on its own after a short interval so it doesn't clutter
//! the sidebar view the user has returned to.
//!
//! The notice carries no timer of its own — wall clocks belong to the
//! event loop. Call [`DetachNotice::show`] when the detach RPC succeeds
//! and [`DetachNotice::tick`] from the main loop's animation tick;
//! anything < ~2 seconds reads as the right duration for a non-blocking
//! confirmation.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

const DEFAULT_TTL: Duration = Duration::from_millis(2000);

/// Default text used when [`DetachNotice::show`] is called without an
/// explicit message. Matches the wording in the task spec
/// (`f2d67ece...` / WEK-20).
pub const DEFAULT_MESSAGE: &str = "会话仍在后台运行";

#[derive(Default)]
pub struct DetachNotice {
    visible_until: Option<Instant>,
    message: String,
    ttl: Duration,
}

impl DetachNotice {
    pub fn new() -> Self {
        Self {
            visible_until: None,
            message: String::new(),
            ttl: DEFAULT_TTL,
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Show the default message for the configured TTL.
    pub fn show(&mut self, now: Instant) {
        self.show_with(now, DEFAULT_MESSAGE);
    }

    /// Show a custom message for the configured TTL.
    pub fn show_with(&mut self, now: Instant, message: impl Into<String>) {
        self.message = message.into();
        self.visible_until = Some(now + self.ttl);
    }

    /// Hide immediately (e.g. user dismissed by pressing Esc).
    pub fn dismiss(&mut self) {
        self.visible_until = None;
    }

    /// Return `true` while the notice should be drawn. Call from render.
    pub fn is_visible(&self, now: Instant) -> bool {
        match self.visible_until {
            Some(deadline) => now < deadline,
            None => false,
        }
    }

    /// Idempotent expiry hook for the main loop's animation tick. Returns
    /// `true` if the notice just transitioned to hidden (so the caller
    /// knows to schedule a repaint).
    pub fn tick(&mut self, now: Instant) -> bool {
        if let Some(deadline) = self.visible_until {
            if now >= deadline {
                self.visible_until = None;
                return true;
            }
        }
        false
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Stateless renderer for the notice. Renders a centered single-row toast
/// over `area`; the caller picks the rect — usually a one-line strip
/// pinned to the bottom of the main area, above the composer.
pub struct DetachNoticeView<'a> {
    notice: &'a DetachNotice,
    now: Instant,
}

impl<'a> DetachNoticeView<'a> {
    pub fn new(notice: &'a DetachNotice, now: Instant) -> Self {
        Self { notice, now }
    }
}

impl<'a> Widget for DetachNoticeView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if !self.notice.is_visible(self.now) || area.width == 0 || area.height == 0 {
            return;
        }
        // Clear the cells under us so we don't blend with whatever the
        // surrounding widget painted first.
        Clear.render(area, buf);
        let style = Style::default().fg(Color::Black).bg(Color::Yellow);
        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        block.render(area, buf);
        let line = Line::from(vec![Span::styled(self.notice.message().to_string(), style)]);
        Paragraph::new(line)
            .alignment(Alignment::Center)
            .render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_by_default() {
        let n = DetachNotice::new();
        assert!(!n.is_visible(Instant::now()));
    }

    #[test]
    fn show_makes_visible_until_ttl() {
        let mut n = DetachNotice::new().with_ttl(Duration::from_millis(500));
        let t0 = Instant::now();
        n.show(t0);
        assert!(n.is_visible(t0));
        assert!(n.is_visible(t0 + Duration::from_millis(499)));
        assert!(!n.is_visible(t0 + Duration::from_millis(500)));
    }

    #[test]
    fn tick_expires_and_signals_repaint() {
        let mut n = DetachNotice::new().with_ttl(Duration::from_millis(100));
        let t0 = Instant::now();
        n.show(t0);
        assert!(!n.tick(t0 + Duration::from_millis(50)));
        assert!(n.tick(t0 + Duration::from_millis(150)));
        // Second tick after expiry must NOT re-signal.
        assert!(!n.tick(t0 + Duration::from_millis(200)));
    }

    #[test]
    fn dismiss_hides_immediately() {
        let mut n = DetachNotice::new();
        let t0 = Instant::now();
        n.show(t0);
        n.dismiss();
        assert!(!n.is_visible(t0));
    }

    #[test]
    fn custom_message_overrides_default() {
        let mut n = DetachNotice::new();
        n.show_with(Instant::now(), "已断开 (会话保留)");
        assert_eq!(n.message(), "已断开 (会话保留)");
    }
}
