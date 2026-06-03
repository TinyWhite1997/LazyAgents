//! Bottom status bar (PRD §5.5 / 架构 §5.6).
//!
//! The bar surfaces six fields, left → right:
//!
//! ```text
//! ● daemon · 3 running · next 02:00 · ↻ cron-nightly · $—/day · main +3 · ⚠ 2 errors
//! ```
//!
//! Field provenance:
//! - `daemon_online` — set false when the IPC subscriber loses its
//!   connection; flips back true on the first `daemon.health` push after
//!   reconnect.
//! - `running` / `errors_last_5m` — last `daemon.health` payload.
//! - `next_cron_label` — derived in the runner from `CronsState::preview`
//!   (the same crate's M3.4 work). A daemon-pushed `crons.next` lands
//!   later (see issue WEK-36 follow-up); the runner just swaps the
//!   source string in when it arrives.
//! - `last_cron_pulse` — most recent `cron.fired` notification; the bar
//!   shows the cron id + fired-at glyph for [`Status::PULSE_TTL`] before
//!   fading.
//! - `today_cost_label` — placeholder until a daemon RPC for cost roll-up
//!   ships; the bar renders `$—/day` when `None`.
//! - `right_context` — free-form trailing text (current session git
//!   branch + dirty count). The binary fills this in; the bar only
//!   right-aligns it.

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Most-recent cron firing pulse, used to flash a brief "↻ cron-id" badge
/// on the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronPulse {
    pub cron_id: String,
    pub fired_at: DateTime<Utc>,
}

/// Snapshot of the values the status bar shows.
///
/// `Status::default()` is the "no daemon yet" snapshot — every numeric
/// field is zero and every optional label is `None`, so the bar renders
/// a red daemon dot and `—` placeholders. The binary replaces this with
/// the first real `daemon.health` payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub daemon_online: bool,
    /// Currently-running sessions across all backends.
    pub running: usize,
    /// "next cron at hh:mm" preview. `None` when no enabled cron has a
    /// resolvable next-fire (empty list, all invalid expressions, …).
    pub next_cron_label: Option<String>,
    /// Free-form right-aligned context (git branch + dirty count, etc.).
    pub right_context: String,
    /// Backend probes in non-`Available` state in the last 5 min. Drives
    /// the red `⚠ N errors` badge that `f` opens into the Errors modal.
    pub errors_last_5m: u32,
    /// Pre-formatted "today's cost" label (e.g. `"$2.31/day"`). `None`
    /// renders the `$—/day` placeholder; a daemon-side roll-up RPC
    /// fills this in post-M3.5.
    pub today_cost_label: Option<String>,
    /// Most recent `cron.fired` pulse, decayed by the renderer once it
    /// is older than [`Self::PULSE_TTL`].
    pub last_cron_pulse: Option<CronPulse>,
}

impl Status {
    /// How long a `cron.fired` pulse stays visible on the bar before the
    /// renderer drops it. Long enough that an observer who glances away
    /// for a few seconds still sees the badge; short enough that an idle
    /// status bar isn't permanently noisy.
    pub const PULSE_TTL: chrono::Duration = chrono::Duration::seconds(5);

    /// Convenience: a snapshot reflecting "we have not heard from the
    /// daemon yet". Used by the binary on startup and by the IPC
    /// subscriber when the connection is being re-established.
    pub fn offline() -> Self {
        Self::default()
    }
}

pub fn render_status(frame: &mut Frame<'_>, area: Rect, status: &Status) {
    render_status_at(frame, area, status, Utc::now())
}

/// Like [`render_status`] but with a caller-supplied `now`. Lets tests
/// pin the pulse-decay deterministically.
pub fn render_status_at(frame: &mut Frame<'_>, area: Rect, status: &Status, now: DateTime<Utc>) {
    let mut left: Vec<Span<'_>> = Vec::with_capacity(16);

    // ● daemon  (green) / ○ daemon  (red).
    left.push(daemon_badge(status.daemon_online));
    push_sep(&mut left);
    left.push(Span::raw(format!("{} running", status.running)));

    // next 02:00 — Crons-derived. Absent ⇒ skip the cell rather than
    // padding so a workspace with no crons doesn't bury the actually-
    // visible fields in placeholders.
    if let Some(cron) = &status.next_cron_label {
        push_sep(&mut left);
        left.push(Span::raw(cron.clone()));
    }

    // ↻ cron-id pulse — visible for PULSE_TTL after `cron.fired`.
    if let Some(pulse) = &status.last_cron_pulse {
        if now.signed_duration_since(pulse.fired_at) < Status::PULSE_TTL {
            push_sep(&mut left);
            left.push(Span::styled(
                format!("↻ {}", pulse.cron_id),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    // Cost ($—/day until a daemon RPC populates it).
    push_sep(&mut left);
    left.push(Span::styled(
        cost_label(status.today_cost_label.as_deref()),
        Style::default().fg(Color::Yellow),
    ));

    // Right context — usually `branch +N` from the focused session.
    if !status.right_context.is_empty() {
        push_sep(&mut left);
        left.push(Span::styled(
            status.right_context.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Error badge. Rendered last so it sits at the right end of the
    // dynamic span list — the field is the user's main attention
    // grabber and benefits from a stable position.
    push_sep(&mut left);
    left.push(errors_badge(status.errors_last_5m));

    let para = Paragraph::new(Line::from(left)).block(Block::default().borders(Borders::TOP));
    frame.render_widget(para, area);
}

fn push_sep<'a>(spans: &mut Vec<Span<'a>>) {
    spans.push(Span::raw("  ·  "));
}

fn daemon_badge<'a>(online: bool) -> Span<'a> {
    if online {
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
    }
}

fn cost_label(today: Option<&str>) -> String {
    match today {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "$—/day".to_string(),
    }
}

fn errors_badge<'a>(n: u32) -> Span<'a> {
    if n == 0 {
        Span::styled(
            "OK",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )
    } else {
        Span::styled(
            format!("⚠ {n} errors  [f]"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_to_text(status: &Status, now: DateTime<Utc>) -> String {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 2);
                render_status_at(f, area, status, now);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn offline_status_shows_red_daemon_and_dash_cost() {
        let txt = render_to_text(&Status::offline(), Utc::now());
        assert!(txt.contains("○ daemon"), "no offline dot: {txt}");
        assert!(txt.contains("0 running"));
        assert!(txt.contains("$—/day"), "no cost placeholder: {txt}");
        // No errors ⇒ dim OK badge, not the red one.
        assert!(txt.contains("OK"), "no OK badge when zero errors: {txt}");
        assert!(!txt.contains("⚠"));
    }

    #[test]
    fn errors_badge_appears_with_f_hint_when_nonzero() {
        let mut s = Status::offline();
        s.daemon_online = true;
        s.errors_last_5m = 2;
        let txt = render_to_text(&s, Utc::now());
        assert!(txt.contains("⚠ 2 errors"), "no errors badge: {txt}");
        assert!(txt.contains("[f]"), "missing f-key hint: {txt}");
    }

    #[test]
    fn cron_pulse_fades_after_ttl() {
        let now = Utc::now();
        let pulse = CronPulse {
            cron_id: "nightly-review".into(),
            fired_at: now - chrono::Duration::seconds(1),
        };
        let mut s = Status {
            last_cron_pulse: Some(pulse),
            ..Status::offline()
        };
        s.daemon_online = true;
        let fresh = render_to_text(&s, now);
        assert!(
            fresh.contains("↻ nightly-review"),
            "no fresh pulse: {fresh}"
        );

        // Same status, fast-forward past the TTL — pulse must drop.
        let later = now + Status::PULSE_TTL + chrono::Duration::seconds(1);
        let faded = render_to_text(&s, later);
        assert!(!faded.contains("↻ nightly-review"), "pulse stuck: {faded}");
    }

    #[test]
    fn populated_label_overrides_dash_cost() {
        let mut s = Status::offline();
        s.daemon_online = true;
        s.today_cost_label = Some("$2.31/day".into());
        let txt = render_to_text(&s, Utc::now());
        assert!(txt.contains("$2.31/day"), "missing cost label: {txt}");
        assert!(!txt.contains("$—/day"));
    }

    #[test]
    fn online_daemon_renders_green_dot_and_running_count() {
        let mut s = Status::offline();
        s.daemon_online = true;
        s.running = 3;
        let txt = render_to_text(&s, Utc::now());
        assert!(txt.contains("● daemon"), "no online dot: {txt}");
        assert!(txt.contains("3 running"));
    }
}
