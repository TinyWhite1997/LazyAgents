//! Acceptance-criteria smoke tests for WEK-20 (M1.6).
//!
//! Each test maps to one row of the issue's verification checklist; if a
//! row gets crossed out without a corresponding test here, the next person
//! to read this file has no way to know whether the criterion is actually
//! exercised. Keep this short and readable.

use la_tui::{Composer, ComposerAction, DetachNotice, ScrollAction, Transcript};
use std::time::{Duration, Instant};

/// 验收 1: 长输出滚动流畅 (10k 行)
#[test]
fn ten_thousand_line_buffer_supports_scroll_and_follow() {
    let mut t = Transcript::new(10_000);
    for i in 0..10_000 {
        t.feed(format!("agent line {i}\n").as_bytes());
    }
    t.clamp_for_viewport(40);
    assert!(t.is_following());
    assert_eq!(t.renderable_len(), 10_000);

    // Walk up the buffer using Ctrl+u, all the way to the top, and back
    // down with Ctrl+d — the operation should be cheap & deterministic.
    for _ in 0..1_000 {
        t.scroll(ScrollAction::HalfPageUp, 40);
    }
    assert!(!t.is_following());
    for _ in 0..1_000 {
        t.scroll(ScrollAction::HalfPageDown, 40);
    }
    assert!(t.is_following());
}

/// 验收 2: ConPTY ANSI 测试 — cursor query / clear / OSC chatter
/// must NOT leak into the transcript.
#[test]
fn conpty_attach_burst_does_not_pollute_transcript() {
    let mut t = Transcript::new(100);
    // Mirrors what `windows-rs` ConPTY emits on attach: cursor home,
    // erase display, cursor position query, OSC window title, then real
    // payload.
    t.feed(b"\x1b[H\x1b[2J\x1b[6n\x1b]0;agent\x07hello\nworld\n");
    let lines: Vec<String> = t.lines().iter().map(|l| l.to_plain_string()).collect();
    assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
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
