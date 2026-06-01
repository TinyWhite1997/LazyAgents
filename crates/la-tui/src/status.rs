//! Bottom status bar (PRD §5.5).
//!
//! M1.5 scope: render the placeholder fields with a fed-in `Status` struct.
//! The daemon-driven fields (cron preview, running count) come live once
//! `events.subscribe` is wired in M3; until then the demo binary pushes a
//! static `Status` so the layout/spacing is reviewable.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Snapshot of the values the status bar shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub daemon_online: bool,
    /// Currently-running sessions across all backends.
    pub running: usize,
    /// "next cron at hh:mm" preview. None ⇒ no cron scheduled (or M3 not
    /// wired yet).
    pub next_cron_label: Option<String>,
    /// Free-form right-aligned context (git branch + dirty count, etc.).
    pub right_context: String,
}

pub fn render_status(frame: &mut Frame<'_>, area: Rect, status: &Status) {
    let badge = if status.daemon_online {
        Span::styled(
            "● daemon",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "○ daemon",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };

    let mut left: Vec<Span<'_>> = vec![
        badge,
        Span::raw("  "),
        Span::raw(format!("{} running", status.running)),
    ];
    if let Some(cron) = &status.next_cron_label {
        left.push(Span::raw("  ·  "));
        left.push(Span::raw(cron.clone()));
    }
    if !status.right_context.is_empty() {
        // Pad with spaces so the right context drifts to the right side. We
        // don't have unicode-width here so this is a rough approximation;
        // good enough for a status line.
        left.push(Span::raw("  ·  "));
        left.push(Span::styled(
            status.right_context.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let para = Paragraph::new(Line::from(left)).block(Block::default().borders(Borders::TOP));
    frame.render_widget(para, area);
}
