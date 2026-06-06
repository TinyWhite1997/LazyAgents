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
            let _ = draw(f, &app, None);
        })
        .expect("first draw");
    // Move down once so a session is selected — the hint bar should then
    // advertise "open".
    app.handle(AppMsg::SidebarDown);
    terminal
        .draw(|f| {
            let _ = draw(f, &app, None);
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
    assert!(
        text.contains("proj-a"),
        "expected proj-a group, got:\n{text}"
    );
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

#[test]
fn backends_panel_renders_grey_state_with_reason_and_docs_url() {
    use la_proto::notifications::{BackendHealth as WireBackendHealth, BackendHealthStatus};
    use la_tui::BackendBadge;

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = App::new(MockSessionSource::fixture());

    // Push a mixed backend snapshot: one Available, one NotInstalled
    // (the WEK-29 acceptance fixture — "卸载 codex 后 TUI 不崩，Codex 分组
    // 显示灰态"), one Unauthenticated.
    let badges = vec![
        BackendBadge::from_wire(&WireBackendHealth {
            id: "claude".into(),
            display_name: "Claude Code".into(),
            status: BackendHealthStatus::Available,
            version: Some("2.1.158".into()),
            reason: None,
            docs_url: None,
            last_probed_at: "2026-06-02T00:00:00Z".into(),
        }),
        BackendBadge::from_wire(&WireBackendHealth {
            id: "codex".into(),
            display_name: "Codex CLI".into(),
            status: BackendHealthStatus::NotInstalled,
            version: None,
            reason: Some("codex not on PATH".into()),
            docs_url: Some("https://example.com/install/codex".into()),
            last_probed_at: "2026-06-02T00:00:00Z".into(),
        }),
        BackendBadge::from_wire(&WireBackendHealth {
            id: "opencode".into(),
            display_name: "OpenCode".into(),
            status: BackendHealthStatus::Unauthenticated,
            version: None,
            reason: Some("not logged in".into()),
            docs_url: Some("https://opencode.ai/login".into()),
            last_probed_at: "2026-06-02T00:00:00Z".into(),
        }),
    ];
    app.handle(AppMsg::BackendsUpdate(badges));
    terminal
        .draw(|f| {
            let _ = draw(f, &app, None);
        })
        .expect("draw with backends snapshot");
    let buf = terminal.backend().buffer().clone();
    let text = buffer_text(&buf);

    // Header is rendered.
    assert!(text.contains("Backends"), "expected Backends panel header");
    // Each display name shows up — the grey-state acceptance is keyed on
    // the user being able to see the backend in the sidebar (so they
    // notice it's missing).
    assert!(
        text.contains("Claude Code"),
        "Available backend should still appear in the panel:\n{text}"
    );
    assert!(
        text.contains("Codex CLI"),
        "uninstalled Codex should still be listed, just grey-stated:\n{text}"
    );
    assert!(text.contains("OpenCode"), "OpenCode row missing");

    // Status labels surface the failure mode without the user having to
    // hover or expand anything.
    assert!(
        text.contains("not installed"),
        "expected `not installed` label for Codex:\n{text}"
    );
    assert!(
        text.contains("not logged in"),
        "expected `not logged in` label for OpenCode"
    );

    // Reason + docs URL (truncated to 28 chars) are surfaced — copying
    // them out of the buffer text confirms the actual *characters* land,
    // not just metadata.
    assert!(
        text.contains("codex not on PATH") || text.contains("codex not on PATH"),
        "reason for Codex should appear somewhere in the panel"
    );
    assert!(
        text.contains("https://example.com/install"),
        "docs_url for Codex should appear in the panel:\n{text}"
    );
}

#[test]
fn backends_panel_placeholder_when_no_snapshot() {
    let buf = render_to_buffer(120, 30);
    let text = buffer_text(&buf);
    // The default fixture doesn't push a backend snapshot, so we render
    // the placeholder instead of a panel full of rows. This pins the
    // "TUI doesn't crash" half of the WEK-29 acceptance.
    assert!(text.contains("Backends"), "panel header still drawn");
    assert!(
        text.contains("no probe yet"),
        "placeholder should explain the empty state:\n{text}"
    );
}
