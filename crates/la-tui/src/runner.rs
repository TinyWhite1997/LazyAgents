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
use crate::health_sub::HealthEvent;
use crate::input::{translate, HitBoxes};
use crate::key_hints::{format_hint_bar, HintRegistry};
use crate::sidebar::{render_backends, render_sidebar, Selection};
use crate::source::SessionSource;
use crate::status::render_status;
use crate::tabs::render_tabs;

/// Run the TUI event loop until the user quits. Returns Ok(()) on normal
/// exit; any I/O or terminal-setup error is propagated so the binary can
/// log it and exit nonzero.
pub fn run<S: SessionSource>(app: App<S>) -> io::Result<()> {
    run_with_health(app, None)
}

/// Same as [`run`] but threads in an external [`HealthEvent`] channel —
/// used by the `la` binary to forward `daemon.health` notifications
/// from [`crate::health_sub::spawn`] into the App as `BackendsUpdate`
/// messages.
pub fn run_with_health<S: SessionSource>(
    mut app: App<S>,
    health_rx: Option<Receiver<HealthEvent>>,
) -> io::Result<()> {
    let mut terminal = setup_terminal()?;
    let res = event_loop(&mut terminal, &mut app, health_rx);
    restore_terminal(&mut terminal)?;
    res
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

fn event_loop<S: SessionSource>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<S>,
    health_rx: Option<Receiver<HealthEvent>>,
) -> io::Result<()> {
    let mut hit = HitBoxes {
        tabs: Vec::new(),
        sidebar: Rect::default(),
        sidebar_scroll_offset: 0,
        tab_bar_row: 0,
    };
    loop {
        terminal.draw(|frame| {
            hit = draw(frame, app);
        })?;
        // Drain any pending health events between renders so a fresh
        // `daemon.health` snapshot is reflected on the very next frame.
        if let Some(rx) = health_rx.as_ref() {
            while let Ok(HealthEvent::Backends(badges)) = rx.try_recv() {
                let _ = app.handle(AppMsg::BackendsUpdate(badges));
            }
        }
        // Poll so the screen refreshes periodically; the 250ms cap also
        // bounds how long a backends snapshot can sit in the channel
        // before the next frame consumes it.
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

/// Lay out the screen and render every pane. Returns the hit boxes the
/// event loop needs to translate mouse clicks.
pub fn draw<S: SessionSource>(frame: &mut Frame<'_>, app: &App<S>) -> HitBoxes {
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
            render_crons_placeholder(frame, sidebar_area, content_area);
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

fn render_crons_placeholder(frame: &mut Frame<'_>, sidebar: Rect, content: Rect) {
    let s = Paragraph::new("Crons land in M3.")
        .block(Block::default().borders(Borders::ALL).title("Crons"));
    frame.render_widget(s, sidebar);
    let c =
        Paragraph::new("Cron scheduler is part of milestone M3 (PRD §5.4). Press Tab to return.")
            .block(Block::default().borders(Borders::ALL).title("Detail"))
            .wrap(Wrap { trim: false });
    frame.render_widget(c, content);
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
