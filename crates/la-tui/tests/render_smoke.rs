//! End-to-end render smoke test using ratatui's `TestBackend`.
//!
//! Ensures the entire layout (tabs + sidebar + content + status + hint bar)
//! draws to a buffer without panicking, and that PRD-required strings
//! (group names, badges, navigation hints) actually appear on screen.

use la_tui::runner::draw;
use la_tui::{App, AppMsg, MockSessionSource};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn render_to_buffer(width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = App::new(MockSessionSource::fixture());
    terminal
        .draw(|f| {
            let _ = draw(f, &app);
        })
        .expect("first draw");
    // Move down once so a session is selected — the hint bar should then
    // advertise "open".
    app.handle(AppMsg::SidebarDown);
    terminal
        .draw(|f| {
            let _ = draw(f, &app);
        })
        .expect("second draw");
    terminal.backend().buffer().clone()
}

fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    let mut out = String::new();
    let area = buf.area();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn full_layout_renders_without_panic() {
    let _ = render_to_buffer(120, 30);
}

#[test]
fn sidebar_shows_project_groups_and_badges() {
    let buf = render_to_buffer(120, 30);
    let text = buffer_text(&buf);
    assert!(text.contains("proj-a"), "expected proj-a group, got:\n{text}");
    assert!(text.contains("proj-b"), "expected proj-b group");
    assert!(text.contains("Archived"), "expected Archived bucket pinned");
    assert!(text.contains("claude"), "expected backend badge");
}

#[test]
fn hint_bar_advertises_primary_action_for_session() {
    let buf = render_to_buffer(120, 30);
    let text = buffer_text(&buf);
    assert!(
        text.contains("open"),
        "hint bar should advertise the primary `⏎ open` action when a session is selected"
    );
}

#[test]
fn tab_bar_shows_both_tabs() {
    let buf = render_to_buffer(120, 30);
    let text = buffer_text(&buf);
    assert!(text.contains("Sessions"));
    assert!(text.contains("Crons"));
}
