//! Minimal event loop: render → wait for crossterm event → translate →
//! dispatch → repeat.
//!
//! The runner is kept small so the bulk of the TUI is testable in
//! isolation: business logic lives in [`crate::app::App`], rendering in
//! [`crate::sidebar`] / [`crate::tabs`] / [`crate::status`], and the
//! translation in [`crate::input`]. This module's only job is to glue
//! crossterm I/O to those layers.

use std::io;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;

use crate::app::{App, AppMsg, AppOutcome, Focus, Modal, Tab};
use crate::crons::{human_label, CronSource, CronsState, EditField};
use crate::input::{translate, HitBoxes};
use crate::key_hints::{format_hint_bar, HintRegistry};
use crate::notif_sub::NotifEvent;
use crate::sidebar::{render_backends, render_sidebar, Selection};
use crate::source::SessionSource;
use crate::status::render_status;
use crate::tabs::render_tabs;

/// Back-compat re-export — pre-WEK-36 callers spelled this as
/// `crate::health_sub::HealthEvent`. New code should use [`NotifEvent`].
pub use crate::notif_sub::HealthEvent;

/// Run the TUI event loop until the user quits. Returns Ok(()) on normal
/// exit; any I/O or terminal-setup error is propagated so the binary can
/// log it and exit nonzero.
pub fn run<S: SessionSource, C: CronSource>(app: App<S, C>) -> io::Result<()> {
    run_with_notifs(app, None)
}

/// Same as [`run`] but threads in an external [`NotifEvent`] channel —
/// used by the `la` binary to forward `daemon.health` / `cron.fired`
/// notifications from [`crate::notif_sub::spawn`] into the App as
/// `BackendsUpdate` / `HealthUpdate` / `CronFiredEvent` / `DaemonOffline`
/// messages, plus to refresh the cron preview each frame.
pub fn run_with_notifs<S: SessionSource, C: CronSource>(
    mut app: App<S, C>,
    notif_rx: Option<Receiver<NotifEvent>>,
) -> io::Result<()> {
    let mut terminal = setup_terminal()?;
    let res = event_loop(&mut terminal, &mut app, notif_rx);
    restore_terminal(&mut terminal)?;
    res
}

/// Back-compat alias for the pre-WEK-36 entry point that only consumed
/// `daemon.health`. New code should call [`run_with_notifs`].
pub fn run_with_health<S: SessionSource, C: CronSource>(
    app: App<S, C>,
    health_rx: Option<Receiver<HealthEvent>>,
) -> io::Result<()> {
    run_with_notifs(app, health_rx)
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        cursor::Show
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop<S: SessionSource, C: CronSource>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<S, C>,
    notif_rx: Option<Receiver<NotifEvent>>,
) -> io::Result<()> {
    let mut hit = HitBoxes {
        tabs: Vec::new(),
        sidebar: Rect::default(),
        sidebar_scroll_offset: 0,
        tab_bar_row: 0,
        tab: Tab::Sessions,
        focus: Focus::Sidebar,
    };
    loop {
        // Push a fresh `now` into the Crons state so the inline
        // "今日/明日" labels refresh each frame without the user typing.
        let now = chrono::Utc::now();
        app.crons.set_now(now);
        // Refresh the status bar's "next cron" label from the local
        // CronsState — we don't have a `crons.list_next` push from the
        // daemon yet, so the TUI derives it from the same `CronPreview`
        // the editor pane is showing. Picks the soonest enabled cron's
        // next fire across the full snapshot.
        app.status.next_cron_label = derive_next_cron_label(&app.crons, now);
        terminal.draw(|frame| {
            hit = draw(frame, app);
        })?;
        // Drain any pending notifications between renders so a fresh
        // health / cron pulse is reflected on the very next frame.
        if let Some(rx) = notif_rx.as_ref() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    NotifEvent::Backends(badges) => {
                        let _ = app.handle(AppMsg::BackendsUpdate(badges));
                    }
                    NotifEvent::Health(h) => {
                        let _ = app.handle(AppMsg::HealthUpdate(h));
                    }
                    NotifEvent::CronFired(p) => {
                        let _ = app.handle(AppMsg::CronFiredEvent(p));
                    }
                    NotifEvent::DaemonOffline => {
                        let _ = app.handle(AppMsg::DaemonOffline);
                    }
                }
            }
        }
        // Poll so the screen refreshes periodically; the 250ms cap also
        // bounds how long a notification can sit in the channel before
        // the next frame consumes it.
        if !crossterm::event::poll(Duration::from_millis(250))? {
            continue;
        }
        let ev = crossterm::event::read()?;
        // Resize doesn't need translation: ratatui's `draw` re-queries the
        // size on the next iteration. Other events go to the translator.
        if let Event::Resize(_, _) = ev {
            continue;
        }
        let msg = match translate(ev, app.modal.as_ref(), &hit) {
            Some(m) => m,
            None => continue,
        };
        match app.handle(msg) {
            AppOutcome::Continue => {}
            AppOutcome::Quit => return Ok(()),
        }
    }
}

/// Walk the cron snapshot for the soonest enabled cron whose
/// expression resolves to a future fire and return a human label
/// (`"next 02:00"` style). Returns `None` for an empty list, all
/// disabled, or all invalid expressions.
///
/// Computed inside the runner (not the App) so the App stays
/// independent of `now` — and so the live daemon-pushed equivalent
/// (post-M3.5 `crons.list_next`) can drop in here without touching
/// `App`.
fn derive_next_cron_label(
    crons: &CronsState,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    use chrono::TimeZone;
    use chrono_tz::Tz;

    let mut best: Option<(chrono::DateTime<chrono::Utc>, String)> = None;
    for c in crons.crons() {
        if !c.enabled {
            continue;
        }
        let preview = crate::crons::CronPreview::compute(&c.cron_expr, &c.tz, now);
        let Some(next) = preview.next else { continue };
        match &best {
            Some((cur, _)) if *cur <= next => {}
            _ => {
                let tz: Tz = c.tz.parse().unwrap_or(chrono_tz::UTC);
                let local = tz.from_utc_datetime(&next.naive_utc());
                let label = format!("next {} ({})", local.format("%H:%M"), tz.name());
                best = Some((next, label));
            }
        }
    }
    best.map(|(_, label)| label)
}

/// Lay out the screen and render every pane. Returns the hit boxes the
/// event loop needs to translate mouse clicks.
pub fn draw<S: SessionSource, C: CronSource>(frame: &mut Frame<'_>, app: &App<S, C>) -> HitBoxes {
    let size = frame.area();

    // Vertical stack: tab bar (3 rows incl. border) · main row (rest) · status (2) · hints (1).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // tab bar
            Constraint::Min(5),    // main area
            Constraint::Length(2), // status bar
            Constraint::Length(1), // hint bar
        ])
        .split(size);

    let tabs_area = chunks[0];
    let main_area = chunks[1];
    let status_area = chunks[2];
    let hint_area = chunks[3];

    let tab_ranges = render_tabs(frame, tabs_area, app.tab);

    // Main area: sidebar (left) + content placeholder (right).
    let main_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(20)])
        .split(main_area);
    let sidebar_area = main_split[0];
    let content_area = main_split[1];

    match app.tab {
        Tab::Sessions => {
            // Split the left column: Backends panel on top, Sessions list
            // below. The Backends panel is sized to fit the current
            // snapshot (1 short header line per available backend, up to
            // 3 lines per grey-stated one). Caps at 12 rows so a fleet
            // of unhealthy backends doesn't crowd the session list.
            let backends_rows = if app.backends.is_empty() {
                3
            } else {
                let raw: usize = app
                    .backends
                    .iter()
                    .map(|b| 1 + b.reason.is_some() as usize + b.docs_url.is_some() as usize)
                    .sum();
                // +2 for the panel border (top + bottom).
                (raw + 2).clamp(4, 12)
            };
            let sidebar_split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(backends_rows as u16), Constraint::Min(3)])
                .split(sidebar_area);
            render_backends(frame, sidebar_split[0], &app.backends);
            render_sidebar(
                frame,
                sidebar_split[1],
                &app.sidebar,
                app.focus == Focus::Sidebar,
            );
            render_content_placeholder(frame, content_area, &app.sidebar.selection());
        }
        Tab::Crons => {
            render_crons(frame, sidebar_area, content_area, &app.crons, app.focus);
        }
    }

    render_status(frame, status_area, &app.status);

    let hints = HintRegistry::for_context(
        app.tab,
        app.focus,
        &app.sidebar.selection(),
        app.modal.clone(),
    );
    let hint_text = format_hint_bar(&hints, hint_area.width as usize);
    let hint_para = Paragraph::new(Line::from(Span::styled(
        hint_text,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::DIM),
    )));
    frame.render_widget(hint_para, hint_area);

    if let Some(modal) = &app.modal {
        render_modal(
            frame,
            size,
            modal,
            &app.sidebar.selection(),
            app.tab,
            app.focus,
        );
    }

    HitBoxes {
        tabs: tab_ranges,
        // Exclude the border so a click on the title/border row is not
        // misrouted to row 0 (review feedback from a906b484).
        sidebar: sidebar_area.inner(Margin {
            vertical: 1,
            horizontal: 1,
        }),
        // Mirror the post-render scroll offset so mouse routing stays in
        // sync with what ratatui's List widget actually drew.
        sidebar_scroll_offset: app.sidebar.scroll_offset(),
        tab_bar_row: tabs_area.y,
        tab: app.tab,
        focus: app.focus,
    }
}

fn render_content_placeholder(frame: &mut Frame<'_>, area: Rect, selection: &Selection) {
    let body = match selection {
        Selection::Empty => {
            // The daemon (M1.7) is the only authority that can create the
            // first project; until it lands, `n` is a no-op on an empty
            // workspace (see [`crate::app::App::on_new_session`]). Surface
            // that so the user is not waiting for a key that does nothing.
            "No sessions yet.\n\nThe `la` daemon (M1.7) creates projects from your working directory on first attach. \
Once a project exists, press `n` here to start a session inside it."
                .to_string()
        }
        Selection::Group { project_id } => {
            format!("Group: {project_id}\n\nPress ⏎ to fold/expand, j/k to navigate.")
        }
        Selection::Session { session_id, .. } => format!(
            "Session: {session_id}\n\nConversation pane lands in M1.6. Press ⏎ to (eventually) open."
        ),
    };
    let para = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title("Detail"))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_crons(
    frame: &mut Frame<'_>,
    list_area: Rect,
    editor_area: Rect,
    state: &CronsState,
    focus: Focus,
) {
    render_crons_list(frame, list_area, state, focus == Focus::Sidebar);
    render_crons_editor(frame, editor_area, state, focus == Focus::Main);
}

fn render_crons_list(frame: &mut Frame<'_>, area: Rect, state: &CronsState, focused: bool) {
    let crons = state.crons();
    let cursor = state.cursor().unwrap_or(0);

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(crons.len());
    if crons.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no crons — press `n` to add one)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        for (i, c) in crons.iter().enumerate() {
            let selected = i == cursor;
            let glyph = if c.enabled { "✓" } else { "○" };
            let glyph_style = if c.enabled {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            // ● badge for "dirty" rows so the user knows a save is
            // pending. The list reflects committed state; the editor
            // pane owns the dirty draft.
            let dirty_badge = if c.dirty { " ●" } else { "" };
            let row_style = if selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {glyph} "), glyph_style),
                Span::styled(format!("{:<18}", truncate(&c.name, 18)), row_style),
                Span::styled(
                    format!(" {}", c.cron_expr),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(dirty_badge, Style::default().fg(Color::Yellow)),
            ]));
        }
    }

    let title = if focused { "Crons*" } else { "Crons" };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        });
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_crons_editor(frame: &mut Frame<'_>, area: Rect, state: &CronsState, focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Editor")
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        });
    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    });
    frame.render_widget(block, area);

    let Some(cron) = state.editor_view() else {
        let para = Paragraph::new(
            "No cron selected.\n\nPress `n` to start a new one, or `j`/`k` to pick a row.",
        )
        .wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
        return;
    };

    let preview = state.preview();
    // Inline "下次：…" hint, red-flagged if the expression is invalid.
    let (preview_line, preview_style) = match preview.error.as_deref() {
        Some(err) => (format!("✗ {err}"), Style::default().fg(Color::Red)),
        None => match preview.next {
            Some(next) => (
                human_label(next, state.now(), &cron.tz),
                Style::default().fg(Color::Green),
            ),
            None => ("下次：—".to_string(), Style::default().fg(Color::Yellow)),
        },
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    // Header row: name + dirty badge + enable state.
    let header_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let dirty_badge = if cron.dirty { "  ● unsaved" } else { "" };
    let enabled_badge = if cron.enabled {
        Span::styled("  [enabled]", Style::default().fg(Color::Green))
    } else {
        Span::styled("  [disabled]", Style::default().fg(Color::DarkGray))
    };
    lines.push(Line::from(vec![
        Span::styled(cron.name.clone(), header_style),
        enabled_badge,
        Span::styled(dirty_badge, Style::default().fg(Color::Yellow)),
    ]));
    lines.push(Line::from(Span::styled(preview_line, preview_style)));
    lines.push(Line::from(""));

    let cur_field = state.field();
    for f in EditField::ALL {
        let active = f == cur_field && focused;
        let marker = if active { "▶ " } else { "  " };
        let label_style = if active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(vec![Span::styled(
            format!("{marker}{}", f.label()),
            label_style,
        )]));
        let body = field_body(f, cron);
        let body_style = if f == EditField::CronExpr && preview.error.is_some() {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // Multiline fields (spawn args, prompt) render as separate lines
        // all indented four columns past the marker so they don't shift
        // the form when the field is short — uniform indent across lines
        // matches what a single-line field renders.
        for ln in body.lines() {
            lines.push(Line::from(vec![Span::styled(
                format!("    {ln}"),
                body_style,
            )]));
        }
        if body.is_empty() {
            lines.push(Line::from(Span::styled(
                "    (empty)",
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

fn field_body(field: EditField, cron: &crate::crons::Cron) -> String {
    match field {
        EditField::Name => cron.name.clone(),
        EditField::Backend => cron.backend_id.clone(),
        EditField::SpawnArgs => cron.spawn_args.join("\n"),
        EditField::CronExpr => cron.cron_expr.clone(),
        EditField::Tz => cron.tz.clone(),
        EditField::Prompt => cron.prompt.clone(),
        EditField::Budget => cron
            .cost_budget_usd_per_day
            .map(|v| format!("{v}"))
            .unwrap_or_default(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn render_modal(
    frame: &mut Frame<'_>,
    full: Rect,
    modal: &Modal,
    selection: &Selection,
    tab: Tab,
    focus: Focus,
) {
    match modal {
        Modal::ConfirmDelete { session_id } => {
            let area = centered(full, 60, 7);
            frame.render_widget(Clear, area);
            let body = format!(
                "Delete session {session_id}?\n\nThis cannot be undone.\n\n[y] confirm   [n / Esc] cancel"
            );
            let para = Paragraph::new(body)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Confirm delete")
                        .border_style(Style::default().fg(Color::Red)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
        Modal::FullHints => {
            let area = centered(full, 60, full.height.saturating_sub(6).min(20));
            frame.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Key bindings — current context")
                .border_style(Style::default().fg(Color::Cyan));
            let inner = area.inner(Margin {
                vertical: 1,
                horizontal: 2,
            });
            frame.render_widget(block, area);
            let hints = HintRegistry::for_context(tab, focus, selection, None);
            let mut lines = Vec::new();
            for h in &hints {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<10}", h.key),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::raw(h.label),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Press Esc or ? to close.",
                Style::default().add_modifier(Modifier::DIM),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Modal::NewSession { project_id } => {
            let area = centered(full, 60, 9);
            frame.render_widget(Clear, area);
            let body = format!(
                "New session in project {project_id}\n\nBackend chooser lands with the daemon (M1.7).\nFor now this modal acknowledges the key binding so the\nUI path is reviewable.\n\n[⏎] close   [Esc] cancel"
            );
            let para = Paragraph::new(body)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("New session")
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
        Modal::ConfirmEnableCron {
            cron_name,
            budget_label,
            next_label,
            ..
        } => {
            let area = centered(full, 70, 10);
            frame.render_widget(Clear, area);
            let body = format!(
                "Enable cron \"{cron_name}\"?\n\nDaily cost budget: {budget_label}\n{next_label}\n\nEnabled crons run unattended and spend on real backends.\n\n[y] enable   [n / Esc] cancel"
            );
            let para = Paragraph::new(body)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Confirm enable cron")
                        .border_style(Style::default().fg(Color::Yellow)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
        Modal::ConfirmDeleteCron { cron_name, .. } => {
            let area = centered(full, 60, 7);
            frame.render_widget(Clear, area);
            let body = format!(
                "Delete cron \"{cron_name}\"?\n\nThis cannot be undone — the daemon will stop scheduling it.\n\n[y] confirm   [n / Esc] cancel"
            );
            let para = Paragraph::new(body)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Confirm delete cron")
                        .border_style(Style::default().fg(Color::Red)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
        Modal::DryRunCron { cron_id, fires } => {
            let area = centered(full, 70, (fires.len() as u16 + 6).min(18));
            frame.render_widget(Clear, area);
            let header = Line::from(vec![Span::styled(
                format!("Next {} fires for {cron_id}", fires.len()),
                Style::default().add_modifier(Modifier::BOLD),
            )]);
            let mut lines: Vec<Line<'_>> = vec![header, Line::from("")];
            for (i, f) in fires.iter().enumerate() {
                lines.push(Line::from(format!("  {:>2}. {f}", i + 1)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc / ⏎ to close.",
                Style::default().add_modifier(Modifier::DIM),
            )));
            let para = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Dry-run")
                    .border_style(Style::default().fg(Color::Cyan)),
            );
            frame.render_widget(para, area);
        }
        Modal::Errors { rows } => {
            // Tall enough to show the rows + a header + a footer hint;
            // each row needs at most 3 lines (status, reason, docs).
            let height = ((rows.len() as u16) * 3 + 6).clamp(7, 22);
            let area = centered(full, 80, height);
            frame.render_widget(Clear, area);
            let mut lines: Vec<Line<'_>> = Vec::new();
            if rows.is_empty() {
                lines.push(Line::from(Span::styled(
                    "No active errors. Backends are all healthy.",
                    Style::default().add_modifier(Modifier::DIM),
                )));
            } else {
                for r in rows {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{:<14}", r.id),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(r.status_label.clone(), Style::default().fg(Color::Yellow)),
                    ]));
                    if let Some(reason) = &r.reason {
                        lines.push(Line::from(Span::styled(
                            format!("    {reason}"),
                            Style::default().add_modifier(Modifier::DIM),
                        )));
                    }
                    if let Some(docs) = &r.docs_url {
                        lines.push(Line::from(Span::styled(
                            format!("    → {docs}"),
                            Style::default().fg(Color::Cyan),
                        )));
                    }
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc / ⏎ / f to close.",
                Style::default().add_modifier(Modifier::DIM),
            )));
            let para = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Errors")
                        .border_style(Style::default().fg(Color::Red)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
    }
}

fn centered(parent: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(parent.width.saturating_sub(2));
    let h = height.min(parent.height.saturating_sub(2));
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Convenience for callers that want to send a synthetic key event in
/// tests (e.g. when wiring an integration test for the runner). Not used
/// inside this crate.
pub fn synth_key(code: KeyCode) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    })
}
