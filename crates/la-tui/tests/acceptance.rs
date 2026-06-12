//! Acceptance-criteria smoke tests for WEK-20 (M1.6).
//!
//! Each test maps to one row of the issue's verification checklist; if a
//! row gets crossed out without a corresponding test here, the next person
//! to read this file has no way to know whether the criterion is actually
//! exercised. Keep this short and readable.

use la_tui::{Composer, ComposerAction, DetachNotice, ScrollAction, TermGrid};
use std::time::{Duration, Instant};

/// 验收 1: 长输出滚动流畅 (10k 行) — the VT100 grid keeps a scrollback ring
/// and scroll inputs walk its offset without panic or runaway cost.
#[test]
fn long_output_scrollback_supports_scroll_and_follow() {
    let mut g = TermGrid::new(40, 80);
    for i in 0..10_000 {
        g.feed(format!("agent line {i}\r\n").as_bytes());
    }
    assert!(g.is_following());

    // Walk up the buffer, all the way to the top, and back down — the
    // operation should be cheap & deterministic.
    for _ in 0..1_000 {
        g.scroll(ScrollAction::HalfPageUp, 40);
    }
    assert!(!g.is_following());
    for _ in 0..1_000 {
        g.scroll(ScrollAction::HalfPageDown, 40);
    }
    assert!(g.is_following());
}

/// 验收 2: ConPTY ANSI 测试 — cursor query / clear / OSC chatter are
/// interpreted by the emulator, not leaked as raw bytes into the grid.
#[test]
fn conpty_attach_burst_is_interpreted_not_leaked() {
    use la_tui::TermGridView;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
    let mut g = TermGrid::new(4, 20);
    // Mirrors what `windows-rs` ConPTY emits on attach: cursor home,
    // erase display, cursor position query, OSC window title, then real
    // payload.
    g.feed(b"\x1b[H\x1b[2J\x1b[6n\x1b]0;agent\x07hello\r\nworld\r\n");
    let area = Rect::new(0, 0, 20, 4);
    let mut buf = Buffer::empty(area);
    TermGridView::new(&g).render(area, &mut buf);
    let row = |y: u16| -> String {
        (0..20)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    };
    assert!(row(0).starts_with("hello"), "row0 was {:?}", row(0));
    assert!(row(1).starts_with("world"), "row1 was {:?}", row(1));
    // The cursor query / OSC title must not appear as literal text.
    assert!(
        !row(0).contains('['),
        "escape leaked into grid: {:?}",
        row(0)
    );
}

/// 验收 3: composer 输入历史、↑↓ 回溯
#[test]
fn composer_history_walk_recalls_past_prompts() {
    let mut c = Composer::default();
    for prompt in ["fix the bug", "run the tests", "ship it"] {
        for ch in prompt.chars() {
            c.insert_char(ch);
        }
        c.submit();
    }
    // Up walks toward older entries; Down toward newer.
    assert_eq!(c.history_prev(), ComposerAction::Repaint);
    assert_eq!(c.text(), "ship it");
    assert_eq!(c.history_prev(), ComposerAction::Repaint);
    assert_eq!(c.text(), "run the tests");
    assert_eq!(c.history_next(), ComposerAction::Repaint);
    assert_eq!(c.text(), "ship it");
    assert_eq!(c.history_next(), ComposerAction::Repaint);
    // Past the newest — buffer is empty again (no stashed draft).
    assert!(c.is_empty());
}

/// Detach notice ≈ 2s self-dismiss per the task spec ("detach 时打印
/// 会话仍在后台运行").
#[test]
fn detach_notice_appears_and_self_dismisses() {
    let mut n = DetachNotice::new().with_ttl(Duration::from_millis(2000));
    let t0 = Instant::now();
    assert!(!n.is_visible(t0));
    n.show(t0);
    assert!(n.is_visible(t0));
    assert_eq!(n.message(), "会话仍在后台运行");
    assert!(!n.is_visible(t0 + Duration::from_millis(2000)));
}

/// Wide-char (CJK) content must render on the grid without panic, placing
/// each 2-wide glyph in its lead cell. M1.6's primary users are
/// Chinese-speaking, so wide-char handling is on the golden path.
#[test]
fn cjk_wide_content_renders_without_panic() {
    use la_tui::TermGridView;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
    let mut g = TermGrid::new(3, 20);
    g.feed("小尾巴".as_bytes());
    let area = Rect::new(0, 0, 20, 3);
    let mut buf = Buffer::empty(area);
    TermGridView::new(&g).render(area, &mut buf);
    // The three CJK glyphs occupy lead cells 0, 2, 4 (each 2 wide).
    let row0: Vec<String> = (0..area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect();
    for needle in ['小', '尾', '巴'] {
        assert!(
            row0.iter().any(|s| s == &needle.to_string()),
            "expected '{needle}' on row 0, got {row0:?}"
        );
    }
}
