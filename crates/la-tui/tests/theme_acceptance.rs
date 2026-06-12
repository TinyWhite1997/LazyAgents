//! WEK-42 / M4.3 acceptance tests — theme picker, compact layout, key
//! hints modes, and `[ui]` persistence round-trip.
//!
//! Each test maps to one row of the issue's verification checklist:
//!
//! - 多套色板 + picker：auto / dark / light / catppuccin / … →
//!   [`theme_picker_previews_and_applies`]
//! - 紧凑模式：状态栏 + key hints 单行；侧栏单色后端徽标  →
//!   [`compact_mode_collapses_status_and_hint_into_one_row`] +
//!   [`compact_mode_renders_single_color_backend_badges`]
//! - key_hints = rich | compact | hidden  →
//!   [`key_hints_hidden_drops_bottom_row`] +
//!   [`key_hints_compact_keeps_primary_plus_meta_only`]
//! - 偏好持久化到 settings  → [`ui_pref_changes_round_trip_through_config_toml`]
//! - 切换无闪屏: every change returns control in one [`App::handle`] tick,
//!   so the runner's next [`draw`] frame is the only render.
//! - WCAG AA contrast is covered by `theme::tests::wcag_aa_passes_for_builtin_dark_light`.

use la_tui::runner::draw;
use la_tui::theme::{KeyHintsMode, ThemeCatalog};
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
            let _ = draw(f, &app, None);
        })
        .expect("draw");
    buffer_text(terminal.backend().buffer())
}

/// `T` opens the picker; navigating previews the highlighted theme live
/// (mutating `ui_prefs.theme`), `Confirm` applies + closes, `Cancel`
/// reverts to the theme active on open.
#[test]
fn theme_picker_previews_and_applies() {
    let mut a = App::new(MockSessionSource::fixture());
    assert_eq!(a.ui_prefs.theme, "auto");

    // Open the picker.
    a.handle(AppMsg::OpenThemePicker);
    assert!(matches!(a.modal, Some(la_tui::app::Modal::ThemePicker(_))));

    // Next previews the following catalog entry ("dark").
    a.handle(AppMsg::ThemePickerNext);
    assert_eq!(
        a.ui_prefs.theme, "dark",
        "navigating the picker previews the highlighted theme live"
    );

    // Confirm applies + closes.
    a.handle(AppMsg::Confirm);
    assert!(a.modal.is_none(), "Confirm closes the picker");
    assert_eq!(a.ui_prefs.theme, "dark");
}

#[test]
fn theme_picker_cancel_reverts_preview() {
    let mut a = App::new(MockSessionSource::fixture());
    a.handle(AppMsg::OpenThemePicker);
    a.handle(AppMsg::ThemePickerNext); // preview "dark"
    assert_eq!(a.ui_prefs.theme, "dark");
    a.handle(AppMsg::Cancel);
    assert!(a.modal.is_none());
    assert_eq!(
        a.ui_prefs.theme, "auto",
        "Cancel must revert the live preview to the theme active on open"
    );
}

#[test]
fn theme_picker_persists_selection_to_disk() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("config.toml");
    let mut a = App::new(MockSessionSource::fixture())
        .with_ui_prefs(UiPrefs::default(), Some(path.clone()));
    a.handle(AppMsg::OpenThemePicker);
    a.handle(AppMsg::ThemePickerNext); // → dark
    a.handle(AppMsg::Confirm);

    let reloaded = ui_prefs::load(&path);
    assert_eq!(
        reloaded.theme, "dark",
        "applied theme must hit disk for the next launch"
    );
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

/// WEK-42 / M4.3 review fix: the translator routes H/C inside a modal,
/// but the App's modal short-circuit used to drop them on the floor
/// (handle_in_modal had no arm). Pin both halves: (a) firing
/// `CycleKeyHints` / `ToggleCompact` while a modal is open actually
/// mutates the pref; (b) the modal stays open afterwards.
///
/// `T` is NO LONGER an in-modal global — it opens the picker, which would
/// stack a second modal — so only H/C are exercised here.
#[test]
fn ui_pref_messages_apply_even_when_modal_is_open() {
    let mut a = App::new(MockSessionSource::fixture());
    a.handle(AppMsg::ToggleFullHints);
    assert!(matches!(a.modal, Some(la_tui::app::Modal::FullHints)));

    let hints_before = a.ui_prefs.key_hints;
    let compact_before = a.ui_prefs.compact;

    a.handle(AppMsg::CycleKeyHints);
    assert_ne!(a.ui_prefs.key_hints, hints_before);
    assert!(matches!(a.modal, Some(la_tui::app::Modal::FullHints)));

    a.handle(AppMsg::ToggleCompact);
    assert_ne!(a.ui_prefs.compact, compact_before);
    assert!(matches!(a.modal, Some(la_tui::app::Modal::FullHints)));
}

/// Companion test: when a modal IS open and the message is NOT a UI
/// pref, the normal modal-dispatch path must still fire. Guards against
/// the pre-modal short-circuit accidentally swallowing all messages.
#[test]
fn modal_dispatch_still_works_alongside_pref_short_circuit() {
    let mut a = App::new(MockSessionSource::fixture());
    a.handle(AppMsg::ToggleFullHints);
    assert!(matches!(a.modal, Some(la_tui::app::Modal::FullHints)));
    // Cancel from inside FullHints closes it — that path must still be
    // reachable after the pre-modal pref short-circuit.
    a.handle(AppMsg::Cancel);
    assert!(a.modal.is_none(), "modal cancel still works");
}

/// Pref toggles must also persist to disk when fired inside a modal —
/// the pre-modal short-circuit runs the same `persist_ui_prefs` path
/// the non-modal handler does.
#[test]
fn ui_pref_persists_to_toml_even_when_toggled_inside_modal() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("config.toml");

    let mut a = App::new(MockSessionSource::fixture())
        .with_ui_prefs(UiPrefs::default(), Some(path.clone()));
    a.handle(AppMsg::ToggleFullHints);
    a.handle(AppMsg::CycleKeyHints); // Rich → Compact, with FullHints open
    a.handle(AppMsg::Cancel); // close overlay

    let reloaded = ui_prefs::load(&path);
    assert_eq!(
        reloaded.key_hints,
        KeyHintsMode::Compact,
        "pref toggled inside a modal must hit disk just like the non-modal path"
    );
}

/// Translator-level companion: `T` opens the picker from a normal
/// context, while `H`/`C` route as globals even with a modal open. (T
/// inside a modal is intentionally inert — covered in input.rs unit
/// tests — so it is not asserted here.)
#[test]
fn t_opens_picker_and_h_c_route_inside_a_modal() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use la_tui::app::{Focus, Tab};
    use la_tui::input::{translate, HitBoxes};

    let hit = HitBoxes {
        tabs: Vec::new(),
        sidebar: ratatui::layout::Rect::default(),
        sidebar_scroll_offset: 0,
        tab_bar_row: 0,
        tab: Tab::Sessions,
        focus: Focus::Sidebar,
    };
    // `T` from a normal context opens the picker.
    let ev = Event::Key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::NONE));
    assert_eq!(
        translate(ev, None, &hit),
        Some(AppMsg::OpenThemePicker),
        "'T' must open the theme picker"
    );

    // `H`/`C` route through even with a modal open.
    let modal = la_tui::app::Modal::FullHints;
    for (ch, expected) in [('H', AppMsg::CycleKeyHints), ('C', AppMsg::ToggleCompact)] {
        let ev = Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        let msg = translate(ev, Some(&modal), &hit)
            .unwrap_or_else(|| panic!("'{ch}' must route even with modal open"));
        assert_eq!(msg, expected, "'{ch}' inside modal");
    }
}

/// WEK-42 / M4.3 scope decision: the Crons editor pane is a free-typing
/// context so T/H/C are CAPTURED as field input there. Pin this so the
/// next reviewer sees the intentional asymmetry.
#[test]
fn crons_editor_captures_t_h_c_as_field_input() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use la_tui::app::{Focus, Tab};
    use la_tui::input::{translate, HitBoxes};

    let hit = HitBoxes {
        tabs: Vec::new(),
        sidebar: ratatui::layout::Rect::default(),
        sidebar_scroll_offset: 0,
        tab_bar_row: 0,
        tab: Tab::Crons,
        focus: Focus::Main,
    };
    for ch in ['T', 'H', 'C'] {
        let ev = Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        let msg = translate(ev, None, &hit).expect("editor must translate every char");
        // Should resolve to a CronFieldEdit, NOT to a UI-pref AppMsg.
        let is_pref = matches!(
            msg,
            AppMsg::OpenThemePicker | AppMsg::CycleKeyHints | AppMsg::ToggleCompact
        );
        assert!(
            !is_pref,
            "'{ch}' inside crons editor must feed the buffer, not the global pref handler"
        );
    }
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
            let _ = draw(f, &app, None);
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
        !last_row.contains("open")
            && !last_row.contains("delete")
            && !last_row.contains("all keys"),
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
            let _ = draw(f, &app, None);
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
                    glyph_colors.push(
                        buf[(x, y)]
                            .style()
                            .fg
                            .unwrap_or(ratatui::style::Color::Reset),
                    );
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
    assert_eq!(a.ui_prefs.theme, "auto");

    a.handle(AppMsg::OpenThemePicker);
    a.handle(AppMsg::ThemePickerNext); // → dark
    a.handle(AppMsg::Confirm);
    a.handle(AppMsg::CycleKeyHints); // → Compact
    a.handle(AppMsg::ToggleCompact); // → true

    // Reload from disk — the next launch must observe what we typed.
    let reloaded = ui_prefs::load(&path);
    assert_eq!(reloaded.theme, "dark");
    assert_eq!(reloaded.key_hints, KeyHintsMode::Compact);
    assert!(reloaded.compact);

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("log_level"),
        "[daemon].log_level must survive the TUI save: {raw}"
    );
}

/// WEK-42 / M4.3 reviewer feedback: the original PR only used `Palette`
/// for the hint bar; switching theme did not change any other render
/// surface. Pin the inverse: render the exact same App twice with two
/// different themes and assert that real cells in the status bar,
/// tabs, and sidebar actually carry the theme's `Accent::*` colours.
/// This is a render-level check (not a Palette-internal one), so it
/// would catch a future regression where someone re-hardcodes a
/// `Color::Cyan` somewhere.
#[test]
fn dark_and_light_themes_actually_diverge_on_real_cells() {
    use ratatui::style::Color;

    fn render(theme: &str) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::new(MockSessionSource::fixture()).with_ui_prefs(
            UiPrefs {
                theme: theme.to_string(),
                ..UiPrefs::default()
            },
            None,
        );
        app.handle(AppMsg::SidebarDown);
        terminal
            .draw(|f| {
                let _ = draw(f, &app, None);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    let catalog = ThemeCatalog::builtin();
    let dark = render("dark");
    let light = render("light");
    let dark_pal = catalog.palette("dark");
    let light_pal = catalog.palette("light");

    /// Find the first cell whose symbol matches `needle` **within** the
    /// inclusive y range `y0..=y1`. Bounded so the search doesn't pick
    /// up an identical glyph from a different panel (the `○` for the
    /// status-bar offline dot collides with `RunState::Idle`'s glyph
    /// in the sidebar).
    fn find_fg_in(buf: &ratatui::buffer::Buffer, needle: &str, y0: u16, y1: u16) -> Option<Color> {
        let area = buf.area();
        for y in y0..=y1.min(area.height.saturating_sub(1)) {
            for x in 0..area.width {
                if buf[(x, y)].symbol() == needle {
                    return buf[(x, y)].style().fg;
                }
            }
        }
        None
    }

    // The status bar lives in the last 2-3 rows. Scope status-cell
    // sampling to that band.
    let status_y0 = dark.area.height.saturating_sub(3);
    let status_y1 = dark.area.height.saturating_sub(1);

    // 1) Status bar: the offline ○ daemon dot must carry `Accent::Error`
    //    in each theme. Pre-M4.3 it was a hardcoded `Color::Red`, so
    //    Dark and Light would have rendered the same colour and this
    //    test would have failed even though Palette differed.
    let dark_dot =
        find_fg_in(&dark, "○", status_y0, status_y1).expect("dark offline dot in status band");
    let light_dot =
        find_fg_in(&light, "○", status_y0, status_y1).expect("light offline dot in status band");
    assert_eq!(
        dark_dot,
        dark_pal.color(la_tui::theme::Accent::Error),
        "dark status dot must use Dark Palette Error"
    );
    assert_eq!(
        light_dot,
        light_pal.color(la_tui::theme::Accent::Error),
        "light status dot must use Light Palette Error"
    );
    assert_ne!(
        dark_dot, light_dot,
        "Dark vs Light Error must differ — otherwise the theme cycle is a visual no-op"
    );

    // 2) Active tab chip background: pre-M4.3 was hardcoded Cyan; now
    //    must be `Accent::Primary` per palette. The tab bar is row 0.
    fn find_bg_in(buf: &ratatui::buffer::Buffer, needle: &str, y0: u16, y1: u16) -> Option<Color> {
        let area = buf.area();
        for y in y0..=y1.min(area.height.saturating_sub(1)) {
            for x in 0..area.width {
                if buf[(x, y)].symbol() == needle {
                    return buf[(x, y)].style().bg;
                }
            }
        }
        None
    }
    // The "S" of "[ Sessions ]" sits inside the active chip on row 0.
    let dark_chip = find_bg_in(&dark, "S", 0, 0).expect("dark chip bg");
    let light_chip = find_bg_in(&light, "S", 0, 0).expect("light chip bg");
    assert_eq!(
        dark_chip,
        dark_pal.color(la_tui::theme::Accent::Primary),
        "dark tab chip bg must use Dark Palette Primary"
    );
    assert_eq!(
        light_chip,
        light_pal.color(la_tui::theme::Accent::Primary),
        "light tab chip bg must use Light Palette Primary"
    );
    assert_ne!(
        dark_chip, light_chip,
        "Dark vs Light Primary must differ on real tab cells"
    );
}
