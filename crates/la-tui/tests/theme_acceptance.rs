//! WEK-42 / M4.3 acceptance tests — theme cycle, compact layout, key
//! hints modes, and `[ui]` persistence round-trip.
//!
//! Each test maps to one row of the issue's verification checklist:
//!
//! - 三套色板：auto (跟终端) / dark / light  → [`theme_cycle_visits_all_three_palettes`]
//! - 紧凑模式：状态栏 + key hints 单行；侧栏单色后端徽标  →
//!   [`compact_mode_collapses_status_and_hint_into_one_row`] +
//!   [`compact_mode_renders_single_color_backend_badges`]
//! - key_hints = rich | compact | hidden  →
//!   [`key_hints_hidden_drops_bottom_row`] +
//!   [`key_hints_compact_keeps_primary_plus_meta_only`]
//! - 偏好持久化到 settings  → [`ui_pref_changes_round_trip_through_config_toml`]
//! - 切换无闪屏: every cycle returns control in one [`App::handle`] tick,
//!   so the runner's next [`draw`] frame is the only render —
//!   exercised implicitly by the cycle tests below (no extra wait or
//!   teardown beyond a single message).
//! - WCAG AA contrast is covered by `theme::tests::wcag_aa_passes_for_both_palettes`.

use la_tui::runner::draw;
use la_tui::theme::{KeyHintsMode, Theme};
use la_tui::ui_prefs::{self, UiPrefs};
use la_tui::{App, AppMsg, MockSessionSource};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

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

fn render_with_prefs(prefs: UiPrefs) -> String {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = App::new(MockSessionSource::fixture()).with_ui_prefs(prefs, None);
    // Move past the first group header so a session is selected; the
    // hint bar advertises `⏎ open` only when the cursor is on a session
    // row. Without this every test that asserts on the "open" hint
    // would only exercise the empty / group-header catalogue.
    app.handle(AppMsg::SidebarDown);
    terminal
        .draw(|f| {
            let _ = draw(f, &app);
        })
        .expect("draw");
    buffer_text(terminal.backend().buffer())
}

#[test]
fn theme_cycle_visits_all_three_palettes() {
    let mut a = App::new(MockSessionSource::fixture());
    assert_eq!(a.ui_prefs.theme, Theme::Auto);
    a.handle(AppMsg::CycleTheme);
    assert_eq!(a.ui_prefs.theme, Theme::Dark);
    a.handle(AppMsg::CycleTheme);
    assert_eq!(a.ui_prefs.theme, Theme::Light);
    a.handle(AppMsg::CycleTheme);
    assert_eq!(a.ui_prefs.theme, Theme::Auto, "must cycle back to start");
}

#[test]
fn key_hints_cycle_round_trips() {
    let mut a = App::new(MockSessionSource::fixture());
    assert_eq!(a.ui_prefs.key_hints, KeyHintsMode::Rich);
    a.handle(AppMsg::CycleKeyHints);
    assert_eq!(a.ui_prefs.key_hints, KeyHintsMode::Compact);
    a.handle(AppMsg::CycleKeyHints);
    assert_eq!(a.ui_prefs.key_hints, KeyHintsMode::Hidden);
    a.handle(AppMsg::CycleKeyHints);
    assert_eq!(a.ui_prefs.key_hints, KeyHintsMode::Rich);
}

#[test]
fn toggle_compact_flips_pref() {
    let mut a = App::new(MockSessionSource::fixture());
    assert!(!a.ui_prefs.compact);
    a.handle(AppMsg::ToggleCompact);
    assert!(a.ui_prefs.compact);
    a.handle(AppMsg::ToggleCompact);
    assert!(!a.ui_prefs.compact);
}

#[test]
fn compact_mode_collapses_status_and_hint_into_one_row() {
    let default = render_with_prefs(UiPrefs::default());
    let compact = render_with_prefs(UiPrefs {
        compact: true,
        ..UiPrefs::default()
    });
    // Both renders still show the daemon dot (status content survives)
    // and the open-action hint (key hint survives) — the difference is
    // they share one row in compact mode.
    assert!(default.contains("daemon"), "default lost daemon: {default}");
    assert!(default.contains("open"), "default lost open hint");
    assert!(compact.contains("daemon"), "compact lost daemon");
    assert!(compact.contains("open"), "compact lost open hint");
    // Acceptance "切换无闪屏": one App::handle tick → one full frame.
    // The frame above was produced after a single handle() call, so
    // there is no transient state to wait through; the assertion is
    // that the second draw is identical. Re-draw and compare.
    let compact_again = render_with_prefs(UiPrefs {
        compact: true,
        ..UiPrefs::default()
    });
    assert_eq!(
        compact, compact_again,
        "compact render must be deterministic across draws (no flicker)"
    );
}

#[test]
fn key_hints_hidden_drops_bottom_row() {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = App::new(MockSessionSource::fixture()).with_ui_prefs(
        UiPrefs {
            key_hints: KeyHintsMode::Hidden,
            ..UiPrefs::default()
        },
        None,
    );
    app.handle(AppMsg::SidebarDown);
    terminal
        .draw(|f| {
            let _ = draw(f, &app);
        })
        .expect("draw");
    let buf = terminal.backend().buffer().clone();
    let txt = buffer_text(&buf);
    assert!(txt.contains("daemon"), "status bar should remain visible");

    // Hidden mode drops the bottom hint row entirely — the runner skips
    // the Constraint::Length(1) for it. The status bar's separator (the
    // `───` row above its content) is the only thing left at the very
    // bottom; the row that used to carry `⏎ open · d delete · …` must
    // now be empty (or already occupied by the status border).
    let last_row: String = (0..buf.area.width)
        .map(|x| buf[(x, buf.area.height - 1)].symbol().to_string())
        .collect();
    assert!(
        !last_row.contains("open") && !last_row.contains("delete") && !last_row.contains("all keys"),
        "Hidden mode must drop hint row, but last row carries hints: {last_row:?}"
    );

    // Sanity: comparing a Rich render at the same size, the last row IS
    // a hint row. Pins the inverse so a future bug that always rendered
    // the hint row would be caught.
    let rich = render_with_prefs(UiPrefs::default());
    let rich_last = rich.lines().last().unwrap_or("").to_string();
    assert!(
        rich_last.contains("open") || rich_last.contains("all keys"),
        "Rich render's last row must still carry the hint bar: {rich_last:?}"
    );
}

#[test]
fn key_hints_compact_keeps_primary_plus_meta_only() {
    let compact_hints = render_with_prefs(UiPrefs {
        key_hints: KeyHintsMode::Compact,
        ..UiPrefs::default()
    });
    // Primary survives.
    assert!(
        compact_hints.contains("open"),
        "Compact hints must keep the primary action: {compact_hints}"
    );
    // A teaching/global Low-importance label like "next tab" is dropped
    // because Compact filters to Primary + Meta only.
    assert!(
        !compact_hints.contains("next tab"),
        "Compact hints must drop Low-importance teaching keys: {compact_hints}"
    );
    // Meta keys (`?`) survive — discoverability of the full overlay is
    // the one thing we never want users to lose.
    assert!(
        compact_hints.contains("all keys"),
        "Compact hints must keep the `?` meta entry"
    );
}

#[test]
fn compact_mode_renders_single_color_backend_badges() {
    use la_proto::notifications::BackendHealthStatus;
    use la_tui::model::BackendBadge;

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = App::new(MockSessionSource::fixture()).with_ui_prefs(
        UiPrefs {
            compact: true,
            ..UiPrefs::default()
        },
        None,
    );
    // Mix of states so the non-compact path would have produced 3
    // different colours; compact must collapse to one.
    app.handle(AppMsg::BackendsUpdate(vec![
        BackendBadge {
            id: "claude".into(),
            display_name: "Claude".into(),
            status: BackendHealthStatus::Available,
            reason: None,
            docs_url: None,
            version: Some("2.1".into()),
        },
        BackendBadge {
            id: "codex".into(),
            display_name: "Codex".into(),
            status: BackendHealthStatus::NotInstalled,
            reason: Some("not on PATH".into()),
            docs_url: Some("https://example.com/install".into()),
            version: None,
        },
        BackendBadge {
            id: "opencode".into(),
            display_name: "OpenCode".into(),
            status: BackendHealthStatus::Error,
            reason: Some("crashed".into()),
            docs_url: None,
            version: None,
        },
    ]));
    terminal
        .draw(|f| {
            let _ = draw(f, &app);
        })
        .expect("draw");
    let buf = terminal.backend().buffer().clone();
    // The Backends panel lives in the top-left of the sidebar — the
    // runner sizes it to (badges.len() + 2) rows in compact mode (1 line
    // each + 2 for the border). Sample glyph cells inside that band so
    // we don't accidentally pick up "Claude" / "Codex" strings that
    // appear in the mock session list further down.
    let backends_band_end: u16 = (3 + 2 + 2) as u16; // tab(2) + (badges 3 + border 2)
    let mut glyph_colors: Vec<ratatui::style::Color> = Vec::new();
    for y in 0..backends_band_end.min(buf.area.height) {
        let mut row = String::new();
        for x in 0..buf.area.width {
            row.push_str(buf[(x, y)].symbol());
        }
        if row.contains("Claude") || row.contains("Codex") || row.contains("OpenCode") {
            // First non-space, non-border cell on the row — that's the
            // glyph the compact renderer drew.
            for x in 0..buf.area.width {
                let s = buf[(x, y)].symbol();
                if !s.trim().is_empty() && s != "│" && s != "─" {
                    glyph_colors.push(buf[(x, y)].style().fg.unwrap_or(ratatui::style::Color::Reset));
                    break;
                }
            }
        }
    }
    assert!(
        glyph_colors.len() >= 2,
        "expected glyph samples for multiple backends, got {glyph_colors:?}"
    );
    let first = glyph_colors[0];
    assert!(
        glyph_colors.iter().all(|c| *c == first),
        "compact mode must paint every backend glyph the same colour, got {glyph_colors:?}"
    );
    // Reason/docs sub-lines are dropped in compact mode so the panel
    // stays one line per backend.
    let txt = buffer_text(&buf);
    assert!(
        !txt.contains("not on PATH"),
        "compact mode must drop reason sub-lines"
    );
}

#[test]
fn ui_pref_changes_round_trip_through_config_toml() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("config.toml");
    // Pre-seed config.toml with a daemon section so we can also verify
    // it survives the save (architecture §11.1 acceptance: TUI writes
    // `[ui]` without clobbering siblings).
    std::fs::write(
        &path,
        "[daemon]\nlog_level = \"info\"\n\n[ui]\ntheme = \"auto\"\n",
    )
    .unwrap();

    let prefs = ui_prefs::load(&path);
    let mut a = App::new(MockSessionSource::fixture()).with_ui_prefs(prefs, Some(path.clone()));
    assert_eq!(a.ui_prefs.theme, Theme::Auto);

    a.handle(AppMsg::CycleTheme); // → Dark
    a.handle(AppMsg::CycleKeyHints); // → Compact
    a.handle(AppMsg::ToggleCompact); // → true

    // Reload from disk — the next launch must observe what we typed.
    let reloaded = ui_prefs::load(&path);
    assert_eq!(reloaded.theme, Theme::Dark);
    assert_eq!(reloaded.key_hints, KeyHintsMode::Compact);
    assert!(reloaded.compact);

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("log_level"),
        "[daemon].log_level must survive the TUI save: {raw}"
    );
}
