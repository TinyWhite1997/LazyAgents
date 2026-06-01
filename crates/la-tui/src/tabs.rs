//! Top tab bar (Sessions / Crons).
//!
//! Tabs themselves are part of [`crate::App`] state; this module just owns
//! the renderer and the mouse hit-test. PRD §5.2: `Tab` / `Shift+Tab` / `1`
//! / `2` cycle; mouse click selects directly.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::Tab;

/// Render the tab bar across `area`. Returns the absolute column ranges
/// each tab occupies so the mouse hit-tester ([`tab_at_column`]) can map a
/// click x-coord to the right tab.
///
/// Note: ratatui has a built-in `Tabs` widget, but we want explicit hit
/// boxes for mouse routing and a custom "[ active ]" visual; rolling our
/// own paragraph keeps the code inspectable.
pub fn render_tabs(frame: &mut Frame<'_>, area: Rect, active: Tab) -> Vec<(Tab, std::ops::Range<u16>)> {
    let mut spans: Vec<Span<'_>> = Vec::new();
    let mut ranges: Vec<(Tab, std::ops::Range<u16>)> = Vec::new();
    let mut cursor = area.x + 1; // +1 for the block border padding
    for tab in Tab::ALL {
        let label = format!(" [ {} ] ", tab.label());
        let style = if tab == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let width = label.chars().count() as u16;
        ranges.push((tab, cursor..(cursor + width)));
        spans.push(Span::styled(label, style));
        cursor += width;
    }
    let para = Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(para, area);
    ranges
}

/// Hit-test: given a mouse-click column, return the tab whose range
/// contains it. `None` for clicks outside any tab.
pub fn tab_at_column(
    column: u16,
    ranges: &[(Tab, std::ops::Range<u16>)],
) -> Option<Tab> {
    ranges
        .iter()
        .find(|(_, r)| r.contains(&column))
        .map(|(t, _)| *t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test_picks_right_tab() {
        // Synthetic ranges; we don't render here.
        let ranges = vec![
            (Tab::Sessions, 1u16..14u16),
            (Tab::Crons, 14u16..23u16),
        ];
        assert_eq!(tab_at_column(2, &ranges), Some(Tab::Sessions));
        assert_eq!(tab_at_column(14, &ranges), Some(Tab::Crons));
        assert_eq!(tab_at_column(99, &ranges), None);
    }
}
