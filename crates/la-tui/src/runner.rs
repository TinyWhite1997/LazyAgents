//! Minimal event loop: render → wait for crossterm event → translate →
//! dispatch → repeat.
//!
//! The runner is kept small so the bulk of the TUI is testable in
//! isolation: business logic lives in [`crate::app::App`], rendering in
//! [`crate::sidebar`] / [`crate::tabs`] / [`crate::status`], and the
//! translation in [`crate::input`]. This module's only job is to glue
//! crossterm I/O to those layers.

use std::io;
use std::path::{Path, PathBuf};
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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;

use crate::app::{App, AppMsg, AppOutcome, AttachStatus, Focus, Modal, Tab};
use crate::attach_pump::{AttachEvent, AttachPump};
use crate::crons::{human_label, CronSource, CronsState, EditField};
use crate::input::{translate, HitBoxes};
use crate::key_hints::{format_hint_bar, Hint, HintRegistry, Importance};
use crate::notif_sub::NotifEvent;
use crate::sidebar::{render_sidebar_themed, Selection};
use crate::source::SessionSource;
use crate::status::{render_status_compact, render_status_themed};
use crate::tabs::render_tabs;
use crate::theme::{Accent, KeyHintsMode, Palette};
use crate::transcript::{Transcript, TranscriptView};

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
    app: App<S, C>,
    notif_rx: Option<Receiver<NotifEvent>>,
) -> io::Result<()> {
    run_with_attach(app, notif_rx, None)
}

/// WEK-92-A3 entry: same as [`run_with_notifs`] but also threads the
/// daemon socket path used to spawn per-session [`AttachPump`]s. The
/// `la` binary calls this so pressing Enter on a session row actually
/// opens a live PTY pane.
pub fn run_with_attach<S: SessionSource, C: CronSource>(
    mut app: App<S, C>,
    notif_rx: Option<Receiver<NotifEvent>>,
    attach_socket: Option<PathBuf>,
) -> io::Result<()> {
    let mut terminal = setup_terminal()?;
    let res = event_loop(&mut terminal, &mut app, notif_rx, attach_socket);
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
    attach_socket: Option<PathBuf>,
) -> io::Result<()> {
    let mut hit = HitBoxes {
        tabs: Vec::new(),
        sidebar: Rect::default(),
        sidebar_scroll_offset: 0,
        tab_bar_row: 0,
        tab: Tab::Sessions,
        focus: Focus::Sidebar,
    };
    // WEK-92-A3: per-session attach runtime. The App owns the *state*
    // (`app.attached`), the runner owns the *I/O* (pump thread,
    // transcript buffer, Ctrl+B detach-prefix latch).
    let mut attach: Option<AttachRuntime> = None;
    // WEK-92-A4.1: last `SessionSource::refresh_generation()` value the
    // runner has surfaced as a `RefreshSessions` dispatch. The bg poll
    // (and any future `sessions.changed` push) bumps the counter; the
    // runner notices on the next frame and re-pulls
    // `source.snapshot()` into the sidebar. Without this hop a daemon-
    // side mutation (a sibling `lad` creating a session, an external
    // archive) would only land after the user pressed a key.
    //
    // Seed at 0 (not at the source's current value): if the bg loop
    // finished its first `sessions.list` between `App::with_sources`
    // (which already pulled an initial snapshot) and us getting here,
    // seeding from the live value would skip the dispatch and the
    // sidebar would stay frozen on whatever was true at construction.
    // The first iteration's redundant refresh costs one Mutex lock +
    // Vec clone; a non-issue compared to the freeze it prevents.
    let mut last_refresh_gen: u64 = 0;
    loop {
        // WEK-92-A4.1: pull the sidebar back into sync with the source
        // BEFORE rendering whenever the bg loop has updated its cache.
        // This is the only path that ferries daemon-side state changes
        // into the App outside of user keypresses — without it the
        // sidebar permanently displays whatever was true at startup.
        // Routing through `AppMsg::RefreshSessions` (instead of calling
        // `app.refresh_sessions()` directly) keeps the App as the
        // single owner of sidebar mutations and lets the unit-test
        // surface stay symmetric with `RefreshSessions`'s other call
        // sites.
        let gen = app.source().refresh_generation();
        if gen != last_refresh_gen {
            last_refresh_gen = gen;
            let _ = app.handle(AppMsg::RefreshSessions);
        }
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

        // Reconcile attach state with the App.
        //
        // * App wants an attach (Some) and we have none → spawn a pump.
        // * App cleared its attach (None) and we still own a pump → tell
        //   the pump to detach and drop it.
        // * App switched sessions while a pump is alive → tear down the
        //   old pump (this should be rare: on_enter() refuses re-entry
        //   into the same session, but a future flow that programmatically
        //   switches sessions would land here).
        reconcile_attach(&mut attach, app, attach_socket.as_deref());

        terminal.draw(|frame| {
            hit = draw(frame, app, attach.as_mut());
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
        // Drain attach pump events between renders. Bytes go straight
        // into the runner-owned transcript; status changes are
        // forwarded into the App as AppMsg variants so unit-tested
        // state lives in one place.
        if let Some(rt) = attach.as_mut() {
            drain_attach(rt, app);
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
        // While the transcript pane is focused, raw key events route to
        // the daemon's PTY via `sessions.write` — they do NOT go through
        // the App's normal modal/key translator. The exception is the
        // detach prefix Ctrl+B (see `AttachRuntime::feed_key`).
        if app.focus == Focus::Transcript && app.modal.is_none() {
            if let (Some(rt), Event::Key(k)) = (attach.as_mut(), &ev) {
                // Mirror the normal input translator's release-event
                // filter (`crate::input::translate`): some terminals
                // (notably Windows) report both Press and Release for
                // every key. Without this gate the Release event would
                // also be encoded and written to the PTY, doubling
                // every keystroke and arming the detach prefix on key-up
                // by mistake.
                if !is_transcript_press(k) {
                    continue;
                }
                match rt.feed_key(*k) {
                    KeyOutcome::Consumed => continue,
                    KeyOutcome::Detach => {
                        let _ = app.handle(AppMsg::Detach);
                        continue;
                    }
                    KeyOutcome::FallThrough => {}
                }
            }
        }
        let msg = match translate(ev, app.modal.as_ref(), &hit) {
            Some(m) => m,
            None => continue,
        };
        match app.handle(msg) {
            AppOutcome::Continue => {}
            AppOutcome::Quit => {
                // Best-effort detach so the daemon eagerly releases our
                // input ownership; the pump thread will close on its
                // own when the channel goes away.
                if let Some(rt) = attach.take() {
                    rt.pump.stop();
                }
                return Ok(());
            }
        }
    }
}

/// Per-attach runtime state owned by the runner. Holds the pump, the
/// transcript ring + VTE parser, and the Ctrl+B detach-prefix latch.
pub struct AttachRuntime {
    pub session_id: String,
    pub pump: AttachPump,
    pub transcript: Transcript,
    /// True after the user pressed Ctrl+B; the next keystroke is the
    /// detach action (or `Ctrl+B` to send a literal Ctrl+B byte).
    detach_armed: bool,
}

/// What the runner should do after a raw key event landed in the
/// transcript pane.
enum KeyOutcome {
    /// The byte was forwarded (or absorbed by the detach prefix latch).
    Consumed,
    /// The user typed the detach gesture (Ctrl+B then `d` / Esc / `.`).
    Detach,
    /// The key has no transcript meaning — fall through to the App's
    /// normal translator.
    FallThrough,
}

impl AttachRuntime {
    /// Translate a key event into the right side effect:
    ///   * Ctrl+B → arm detach prefix
    ///   * Ctrl+B then `d` / `Esc` / `.` → detach
    ///   * Ctrl+B then Ctrl+B → send a literal Ctrl+B byte
    ///   * any other key → encode and forward to the daemon
    fn feed_key(&mut self, k: KeyEvent) -> KeyOutcome {
        // Detach prefix takes priority. We only arm on Ctrl+B (not on
        // `Ctrl+b`'s lowercase shadow because crossterm normalizes both
        // to KeyCode::Char('b') with CONTROL set).
        if self.detach_armed {
            self.detach_armed = false;
            match k.code {
                KeyCode::Char('d') | KeyCode::Esc | KeyCode::Char('.') => {
                    return KeyOutcome::Detach;
                }
                KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    // User asked for a literal Ctrl+B byte (0x02).
                    self.pump.write(vec![0x02]);
                    return KeyOutcome::Consumed;
                }
                _ => {
                    // Any other key cancels the prefix and is dropped
                    // (so an accidental Ctrl+B doesn't fire a stray
                    // character into the agent).
                    return KeyOutcome::Consumed;
                }
            }
        }
        if let KeyCode::Char('b') = k.code {
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                self.detach_armed = true;
                return KeyOutcome::Consumed;
            }
        }
        let bytes = match encode_key(k) {
            Some(b) => b,
            None => return KeyOutcome::FallThrough,
        };
        self.pump.write(bytes);
        KeyOutcome::Consumed
    }
}

fn reconcile_attach<S: SessionSource, C: CronSource>(
    attach: &mut Option<AttachRuntime>,
    app: &App<S, C>,
    socket: Option<&Path>,
) {
    match (&app.attached, attach.as_ref()) {
        (Some(att), None) => {
            // App wants an attach and we have none. Need a socket to spawn.
            let Some(socket) = socket else { return };
            let pump = AttachPump::spawn(socket, &att.session_id);
            *attach = Some(AttachRuntime {
                session_id: att.session_id.clone(),
                pump,
                transcript: Transcript::default(),
                detach_armed: false,
            });
        }
        (Some(att), Some(rt)) if att.session_id != rt.session_id => {
            // Session changed under the runner. Tear down the old pump
            // and spawn a fresh one.
            if let Some(old) = attach.take() {
                old.pump.stop();
            }
            if let Some(socket) = socket {
                let pump = AttachPump::spawn(socket, &att.session_id);
                *attach = Some(AttachRuntime {
                    session_id: att.session_id.clone(),
                    pump,
                    transcript: Transcript::default(),
                    detach_armed: false,
                });
            }
        }
        (None, Some(_)) => {
            // App cleared the attach (user pressed Ctrl+B d, or the
            // pump emitted Closed and the App ran the AttachClosed
            // handler). Tell the pump to stop and drop it.
            if let Some(old) = attach.take() {
                old.pump.stop();
            }
        }
        _ => {}
    }
}

fn drain_attach<S: SessionSource, C: CronSource>(rt: &mut AttachRuntime, app: &mut App<S, C>) {
    // Pull every pending event before returning so the next render
    // reflects the freshest bytes. The pump pushes everything through
    // an mpsc channel so try_recv() is cheap.
    while let Ok(ev) = rt.pump.rx.try_recv() {
        match ev {
            AttachEvent::Connected {
                session_id,
                snapshot_seq,
                input_acquired,
            } => {
                let _ = app.handle(AppMsg::AttachConnected {
                    session_id,
                    snapshot_seq,
                    input_acquired,
                });
            }
            AttachEvent::Bytes { bytes, .. } => {
                rt.transcript.feed(&bytes);
            }
            AttachEvent::Gap {
                from_seq,
                to_seq,
                dropped_bytes,
                ..
            } => {
                // The transcript widget already renders a "…N lines
                // dropped" hint when the scrollback cap evicts old
                // bytes; for a wire-level gap we surface a short toast
                // so the user knows the stream skipped, then keep going.
                rt.transcript.feed(
                    format!(
                        "\n── gap: skipped {dropped_bytes} bytes (seq {from_seq}..={to_seq}) ──\n"
                    )
                    .as_bytes(),
                );
            }
            AttachEvent::State { state, reason, .. } => {
                let line = match reason {
                    Some(r) => format!("\n── state: {state} ({r}) ──\n"),
                    None => format!("\n── state: {state} ──\n"),
                };
                rt.transcript.feed(line.as_bytes());
            }
            AttachEvent::Disconnected {
                reason,
                will_reconnect,
            } => {
                let _ = app.handle(AppMsg::AttachDisconnected {
                    session_id: rt.session_id.clone(),
                    reason,
                    will_reconnect,
                });
            }
            AttachEvent::Closed => {
                let _ = app.handle(AppMsg::AttachClosed);
            }
        }
    }
}

/// True when a transcript-focus key event should be forwarded to the
/// daemon PTY. Returns false for `KeyEventKind::Release` (and any other
/// non-press kind), matching the filter applied by the normal input
/// translator in [`crate::input::translate`]. Some terminals — Windows
/// is the canonical offender — report both Press and Release for every
/// keystroke; without this gate every typed character would double on
/// the PTY and a key release of `b` would arm the Ctrl+B detach prefix.
fn is_transcript_press(k: &KeyEvent) -> bool {
    matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

/// Translate a crossterm [`KeyEvent`] into the byte sequence the daemon
/// should write into the PTY master. Returns `None` for keys with no
/// terminal meaning (function keys, media keys, etc.); the caller falls
/// through to the App's normal translator so global keys still work.
fn encode_key(k: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl maps ASCII letters and a few symbols to their
                // 0x01..0x1f counterpart; non-letter Ctrl chords fall
                // through unmodified.
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_alphabetic() {
                    out.push((lower as u8) - b'`');
                } else {
                    match c {
                        '@' => out.push(0x00),
                        '[' => out.push(0x1b),
                        '\\' => out.push(0x1c),
                        ']' => out.push(0x1d),
                        '^' => out.push(0x1e),
                        '_' => out.push(0x1f),
                        ' ' => out.push(0x00),
                        _ => {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        _ => return None,
    }
    Some(out)
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
///
/// `attach` is the runner-owned attach runtime (transcript + pump), if any.
/// When `Some` and the App is on Sessions with `app.attached` set, the
/// content area renders the transcript instead of the placeholder.
pub fn draw<S: SessionSource, C: CronSource>(
    frame: &mut Frame<'_>,
    app: &App<S, C>,
    attach: Option<&mut AttachRuntime>,
) -> HitBoxes {
    let size = frame.area();

    // WEK-42 / M4.3: bottom row layout depends on `[ui]`.
    //
    // | compact | key_hints | rows                                  |
    // |---------|-----------|---------------------------------------|
    // |  false  | Rich      | status (2) + hint (1) — pre-M4.3      |
    // |  false  | Compact   | status (2) + hint (1) — same height,  |
    // |         |           |   bar truncates to Primary only       |
    // |  false  | Hidden    | status (2) — hint row dropped         |
    // |  true   | Rich      | status+hint merged into 1 row         |
    // |  true   | Compact   | status+hint merged into 1 row         |
    // |  true   | Hidden    | status (1) — hint dropped, no border  |
    //
    // The merged variant repaints with `render_status_with_layout`
    // (status spans only) and appends a single hint span trail. This
    // reclaims one full vertical line, which on small terminals is
    // what makes the conversation pane usable.
    let palette = Palette::for_theme(app.ui_prefs.theme);
    let key_hints_mode = app.ui_prefs.key_hints;
    let compact = app.ui_prefs.compact;
    let show_hints_row =
        matches!(key_hints_mode, KeyHintsMode::Rich | KeyHintsMode::Compact) && !compact;

    let status_height: u16 = if compact { 1 } else { 2 };
    let hint_height: u16 = if show_hints_row { 1 } else { 0 };

    let mut constraints: Vec<Constraint> = Vec::with_capacity(4);
    constraints.push(Constraint::Length(2)); // tab bar
    constraints.push(Constraint::Min(5)); // main
    constraints.push(Constraint::Length(status_height));
    if hint_height > 0 {
        constraints.push(Constraint::Length(hint_height));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(size);

    let tabs_area = chunks[0];
    let main_area = chunks[1];
    let status_area = chunks[2];
    let hint_area = if hint_height > 0 {
        Some(chunks[3])
    } else {
        None
    };

    let tab_ranges = render_tabs(frame, tabs_area, app.tab, &palette);

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
            } else if compact {
                // Compact: every backend is one row, no reason / docs.
                (app.backends.len() + 2).clamp(4, 8)
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
            crate::sidebar::render_backends_with_style(
                frame,
                sidebar_split[0],
                &app.backends,
                compact,
                &palette,
            );
            render_sidebar_themed(
                frame,
                sidebar_split[1],
                &app.sidebar,
                app.focus == Focus::Sidebar,
                &palette,
            );
            match (app.attached.as_ref(), attach) {
                (Some(att), Some(rt)) if att.session_id == rt.session_id => {
                    render_attach_pane(frame, content_area, att, rt, &palette);
                }
                _ => {
                    render_content_placeholder(
                        frame,
                        content_area,
                        &app.sidebar.selection(),
                        &palette,
                    );
                }
            }
        }
        Tab::Crons => {
            render_crons(
                frame,
                sidebar_area,
                content_area,
                &app.crons,
                app.focus,
                &palette,
            );
        }
    }

    // Hints catalogue for the current context. Computed once and shared
    // between the inline-into-status path (compact mode) and the
    // standalone hint row.
    let hints_full = HintRegistry::for_context(
        app.tab,
        app.focus,
        &app.sidebar.selection(),
        app.modal.clone(),
    );
    let hints_for_bar: Vec<Hint> = if matches!(key_hints_mode, KeyHintsMode::Compact) {
        // Compact: keep just the primary action (top of the catalogue)
        // and meta keys, so users still see `q quit` / `? all keys`.
        hints_full
            .iter()
            .filter(|h| h.importance == Importance::Primary || h.importance == Importance::Meta)
            .cloned()
            .collect()
    } else {
        hints_full.clone()
    };

    if compact {
        // One merged row: status spans + ` ▏ ` + hint bar, both styled
        // off the palette so theme changes propagate consistently.
        render_status_compact(frame, status_area, &app.status, &palette);
        if matches!(key_hints_mode, KeyHintsMode::Rich | KeyHintsMode::Compact) {
            let hint_text = format_hint_bar(&hints_for_bar, (status_area.width / 2) as usize);
            let inline = Paragraph::new(Line::from(vec![
                Span::raw("  ▏ "),
                Span::styled(
                    hint_text,
                    Style::default()
                        .fg(palette.color(Accent::Muted))
                        .add_modifier(Modifier::DIM),
                ),
            ]))
            .alignment(ratatui::layout::Alignment::Right);
            // Overlay on the same row as the status bar. ratatui paints
            // status first; the right-aligned paragraph repaints the
            // right half. The status renderer left-aligns its content
            // so the overlap is empty space.
            frame.render_widget(inline, status_area);
        }
    } else {
        render_status_themed(frame, status_area, &app.status, &palette);
        if let Some(area) = hint_area {
            let hint_text = format_hint_bar(&hints_for_bar, area.width as usize);
            let hint_para = Paragraph::new(Line::from(Span::styled(
                hint_text,
                Style::default()
                    .fg(palette.color(Accent::Muted))
                    .add_modifier(Modifier::DIM),
            )));
            frame.render_widget(hint_para, area);
        }
    }

    if let Some(modal) = &app.modal {
        render_modal(
            frame,
            size,
            modal,
            &app.sidebar.selection(),
            app.tab,
            app.focus,
            &palette,
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

fn render_content_placeholder(
    frame: &mut Frame<'_>,
    area: Rect,
    selection: &Selection,
    palette: &Palette,
) {
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
        Selection::Session { session_id, .. } => {
            format!("Session: {session_id}\n\nPress ⏎ to attach to the live PTY (WEK-92-A3).")
        }
    };
    let para = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Detail")
                .border_style(Style::default().fg(palette.color(Accent::Muted))),
        )
        .style(Style::default().fg(palette.color(Accent::Body)))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

/// Render the live transcript pane for an active attach (WEK-92-A3).
/// Top: status header (`session-id · connected / connecting / 已断开`).
/// Body: [`TranscriptView`] over the runner-owned transcript ring.
/// Bottom hint inside the block: detach gesture.
fn render_attach_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    att: &crate::app::AttachedSession,
    rt: &mut AttachRuntime,
    palette: &Palette,
) {
    let muted = palette.color(Accent::Muted);
    let primary = palette.color(Accent::Primary);
    let ok = palette.color(Accent::Ok);
    let warn = palette.color(Accent::Warn);
    let err = palette.color(Accent::Error);
    let body_color = palette.color(Accent::Body);
    let (status_label, status_color) = match &att.status {
        AttachStatus::Connecting => ("连接中…", muted),
        AttachStatus::Connected {
            input_acquired: true,
        } => ("connected · input owned", ok),
        AttachStatus::Connected {
            input_acquired: false,
        } => ("connected · read-only", warn),
        AttachStatus::Disconnected { .. } => ("已断开", err),
    };
    // The reason / retry suffix is rendered as a second styled span on
    // the header line (see below). The border title stays terse so it
    // does not jitter as the reason text changes length.
    let detached_extra: Option<String> = match &att.status {
        AttachStatus::Disconnected {
            reason,
            will_reconnect,
        } => Some(if *will_reconnect {
            format!(" — {reason} (重试中…)")
        } else {
            format!(" — {reason}")
        }),
        _ => None,
    };
    let title = format!("Session {} [{}]", short_id(&att.session_id), status_label);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(
            if matches!(att.status, AttachStatus::Connected { .. }) {
                primary
            } else {
                status_color
            },
        ));
    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    frame.render_widget(block, area);

    // Header line above the transcript: status + detach hint.
    if inner.height >= 1 {
        let mut spans = vec![Span::styled(
            status_label.to_string(),
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(extra) = detached_extra {
            spans.push(Span::styled(extra, Style::default().fg(muted)));
        }
        spans.push(Span::styled(
            "   Ctrl+B d 退出",
            Style::default().fg(muted).add_modifier(Modifier::DIM),
        ));
        let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(Paragraph::new(Line::from(spans)), header_area);
    }
    if inner.height >= 2 {
        let body_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
        let widget = TranscriptView::new(&mut rt.transcript).style(Style::default().fg(body_color));
        frame.render_widget(widget, body_area);
    }
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

fn render_crons(
    frame: &mut Frame<'_>,
    list_area: Rect,
    editor_area: Rect,
    state: &CronsState,
    focus: Focus,
    palette: &Palette,
) {
    render_crons_list(frame, list_area, state, focus == Focus::Sidebar, palette);
    render_crons_editor(frame, editor_area, state, focus == Focus::Main, palette);
}

fn render_crons_list(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &CronsState,
    focused: bool,
    palette: &Palette,
) {
    let crons = state.crons();
    let cursor = state.cursor().unwrap_or(0);

    let ok = palette.color(Accent::Ok);
    let muted = palette.color(Accent::Muted);
    let primary = palette.color(Accent::Primary);
    let warn = palette.color(Accent::Warn);

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(crons.len());
    if crons.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no crons — press `n` to add one)",
            Style::default().fg(muted).add_modifier(Modifier::DIM),
        )));
    } else {
        for (i, c) in crons.iter().enumerate() {
            let selected = i == cursor;
            let glyph = if c.enabled { "✓" } else { "○" };
            let glyph_style = if c.enabled {
                Style::default().fg(ok)
            } else {
                Style::default().fg(muted)
            };
            // ● badge for "dirty" rows so the user knows a save is
            // pending. The list reflects committed state; the editor
            // pane owns the dirty draft.
            let dirty_badge = if c.dirty { " ●" } else { "" };
            let row_style = if selected {
                Style::default()
                    .fg(palette.color(Accent::OnAccent))
                    .bg(primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.color(Accent::Body))
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {glyph} "), glyph_style),
                Span::styled(format!("{:<18}", truncate(&c.name, 18)), row_style),
                Span::styled(format!(" {}", c.cron_expr), Style::default().fg(primary)),
                Span::styled(dirty_badge, Style::default().fg(warn)),
            ]));
        }
    }

    let title = if focused { "Crons*" } else { "Crons" };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(if focused {
            Style::default().fg(primary)
        } else {
            Style::default().fg(muted)
        });
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_crons_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &CronsState,
    focused: bool,
    palette: &Palette,
) {
    let primary = palette.color(Accent::Primary);
    let muted = palette.color(Accent::Muted);
    let warn = palette.color(Accent::Warn);
    let err = palette.color(Accent::Error);
    let ok = palette.color(Accent::Ok);
    let body = palette.color(Accent::Body);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Editor")
        .border_style(if focused {
            Style::default().fg(primary)
        } else {
            Style::default().fg(muted)
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
        .style(Style::default().fg(body))
        .wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
        return;
    };

    let preview = state.preview();
    // Inline "下次：…" hint, red-flagged if the expression is invalid.
    let (preview_line, preview_style) = match preview.error.as_deref() {
        Some(e) => (format!("✗ {e}"), Style::default().fg(err)),
        None => match preview.next {
            Some(next) => (
                human_label(next, state.now(), &cron.tz),
                Style::default().fg(ok),
            ),
            None => ("下次：—".to_string(), Style::default().fg(warn)),
        },
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    // Header row: name + dirty badge + enable state.
    let header_style = Style::default().fg(body).add_modifier(Modifier::BOLD);
    let dirty_badge = if cron.dirty { "  ● unsaved" } else { "" };
    let enabled_badge = if cron.enabled {
        Span::styled("  [enabled]", Style::default().fg(ok))
    } else {
        Span::styled("  [disabled]", Style::default().fg(muted))
    };
    lines.push(Line::from(vec![
        Span::styled(cron.name.clone(), header_style),
        enabled_badge,
        Span::styled(dirty_badge, Style::default().fg(warn)),
    ]));
    lines.push(Line::from(Span::styled(preview_line, preview_style)));
    lines.push(Line::from(""));

    let cur_field = state.field();
    for f in EditField::ALL {
        let active = f == cur_field && focused;
        let marker = if active { "▶ " } else { "  " };
        let label_style = if active {
            Style::default().fg(primary).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(muted).add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(vec![Span::styled(
            format!("{marker}{}", f.label()),
            label_style,
        )]));
        let body_text = field_body(f, cron);
        let body_style = if f == EditField::CronExpr && preview.error.is_some() {
            Style::default().fg(err).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(body)
        };
        for ln in body_text.lines() {
            lines.push(Line::from(vec![Span::styled(
                format!("    {ln}"),
                body_style,
            )]));
        }
        if body_text.is_empty() {
            lines.push(Line::from(Span::styled(
                "    (empty)",
                Style::default().fg(muted).add_modifier(Modifier::DIM),
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
    palette: &Palette,
) {
    let primary = palette.color(Accent::Primary);
    let warn = palette.color(Accent::Warn);
    let err = palette.color(Accent::Error);
    let muted = palette.color(Accent::Muted);
    let body = palette.color(Accent::Body);
    match modal {
        Modal::ConfirmDelete { session_id } => {
            let area = centered(full, 60, 7);
            frame.render_widget(Clear, area);
            let body_text = format!(
                "Delete session {session_id}?\n\nThis cannot be undone.\n\n[y] confirm   [n / Esc] cancel"
            );
            let para = Paragraph::new(body_text)
                .style(Style::default().fg(body))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Confirm delete")
                        .border_style(Style::default().fg(err)),
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
                .border_style(Style::default().fg(primary));
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
                        Style::default().fg(warn).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(h.label, Style::default().fg(body)),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Press Esc or ? to close.",
                Style::default().fg(muted).add_modifier(Modifier::DIM),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        Modal::NewSession(draft) => {
            render_new_session_modal(frame, full, draft, body, muted, primary, warn, err);
        }
        Modal::ConfirmEnableCron {
            cron_name,
            budget_label,
            next_label,
            ..
        } => {
            let area = centered(full, 70, 10);
            frame.render_widget(Clear, area);
            let body_text = format!(
                "Enable cron \"{cron_name}\"?\n\nDaily cost budget: {budget_label}\n{next_label}\n\nEnabled crons run unattended and spend on real backends.\n\n[y] enable   [n / Esc] cancel"
            );
            let para = Paragraph::new(body_text)
                .style(Style::default().fg(body))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Confirm enable cron")
                        .border_style(Style::default().fg(warn)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
        Modal::ConfirmDeleteCron { cron_name, .. } => {
            let area = centered(full, 60, 7);
            frame.render_widget(Clear, area);
            let body_text = format!(
                "Delete cron \"{cron_name}\"?\n\nThis cannot be undone — the daemon will stop scheduling it.\n\n[y] confirm   [n / Esc] cancel"
            );
            let para = Paragraph::new(body_text)
                .style(Style::default().fg(body))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Confirm delete cron")
                        .border_style(Style::default().fg(err)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
        Modal::DryRunCron { cron_id, fires } => {
            let area = centered(full, 70, (fires.len() as u16 + 6).min(18));
            frame.render_widget(Clear, area);
            let header = Line::from(vec![Span::styled(
                format!("Next {} fires for {cron_id}", fires.len()),
                Style::default().fg(body).add_modifier(Modifier::BOLD),
            )]);
            let mut lines: Vec<Line<'_>> = vec![header, Line::from("")];
            for (i, f) in fires.iter().enumerate() {
                lines.push(Line::from(Span::styled(
                    format!("  {:>2}. {f}", i + 1),
                    Style::default().fg(body),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc / ⏎ to close.",
                Style::default().fg(muted).add_modifier(Modifier::DIM),
            )));
            let para = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Dry-run")
                    .border_style(Style::default().fg(primary)),
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
                    Style::default().fg(muted).add_modifier(Modifier::DIM),
                )));
            } else {
                for r in rows {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{:<14}", r.id),
                            Style::default().fg(err).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(r.status_label.clone(), Style::default().fg(warn)),
                    ]));
                    if let Some(reason) = &r.reason {
                        lines.push(Line::from(Span::styled(
                            format!("    {reason}"),
                            Style::default().fg(muted).add_modifier(Modifier::DIM),
                        )));
                    }
                    if let Some(docs) = &r.docs_url {
                        lines.push(Line::from(Span::styled(
                            format!("    → {docs}"),
                            Style::default().fg(primary),
                        )));
                    }
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc / ⏎ / f to close.",
                Style::default().fg(muted).add_modifier(Modifier::DIM),
            )));
            let para = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Errors")
                        .border_style(Style::default().fg(err)),
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

/// Render the WEK-94 / A2 New-session modal: backend picker + prompt
/// buffer + worktree toggle, all driven from
/// [`crate::app::NewSessionDraft`].
#[allow(clippy::too_many_arguments)]
fn render_new_session_modal(
    frame: &mut Frame,
    full: Rect,
    draft: &crate::app::NewSessionDraft,
    body: ratatui::style::Color,
    muted: ratatui::style::Color,
    primary: ratatui::style::Color,
    warn: ratatui::style::Color,
    err: ratatui::style::Color,
) {
    use crate::app::NewSessionField;
    let area = centered(full, 72, 18);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            "New session — project {} {}",
            draft.project_id,
            if draft.project_dir.is_empty() {
                "".to_string()
            } else {
                format!("({})", draft.project_dir)
            }
        ))
        .border_style(Style::default().fg(primary));
    frame.render_widget(block, area);
    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    });

    let mut lines: Vec<Line<'_>> = Vec::new();

    // --- Backend row ----------------------------------------------------
    let backend_focus = matches!(draft.field, NewSessionField::Backend);
    let backend_label_style = if backend_focus {
        Style::default().fg(primary).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(muted)
    };
    let mut backend_spans = vec![Span::styled("Backend  ", backend_label_style)];
    if draft.backends.is_empty() {
        backend_spans.push(Span::styled(
            "(no available backend — start a backend first)",
            Style::default().fg(err),
        ));
    } else {
        for (i, b) in draft.backends.iter().enumerate() {
            let style = if i == draft.backend_idx {
                Style::default().fg(body).add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(body)
            };
            backend_spans.push(Span::styled(format!(" {b} "), style));
        }
        if backend_focus {
            backend_spans.push(Span::styled(
                "   ←/→",
                Style::default().fg(muted).add_modifier(Modifier::DIM),
            ));
        }
    }
    lines.push(Line::from(backend_spans));
    lines.push(Line::from(""));

    // --- Prompt block ---------------------------------------------------
    let prompt_focus = matches!(draft.field, NewSessionField::Prompt);
    let prompt_label_style = if prompt_focus {
        Style::default().fg(primary).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(muted)
    };
    lines.push(Line::from(vec![Span::styled(
        "Prompt   ",
        prompt_label_style,
    )]));
    if draft.prompt.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (type the prompt sent to the agent on start — Enter inserts a newline)",
            Style::default().fg(muted).add_modifier(Modifier::DIM),
        )));
    } else {
        for raw in draft.prompt.split('\n') {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(raw.to_string(), Style::default().fg(body)),
            ]));
        }
    }
    if prompt_focus {
        // Cursor caret so the user sees where their next char lands.
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "▌",
                Style::default().fg(primary).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(""));

    // --- Worktree toggle ------------------------------------------------
    let wt_focus = matches!(draft.field, NewSessionField::Worktree);
    let wt_label_style = if wt_focus {
        Style::default().fg(primary).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(muted)
    };
    let wt_value = if draft.worktree { "[x]" } else { "[ ]" };
    let mut wt_spans = vec![
        Span::styled("Worktree ", wt_label_style),
        Span::styled(
            format!(" {wt_value} fresh git worktree for this session"),
            Style::default().fg(body),
        ),
    ];
    if wt_focus {
        wt_spans.push(Span::styled(
            "   space",
            Style::default().fg(muted).add_modifier(Modifier::DIM),
        ));
    }
    lines.push(Line::from(wt_spans));

    // --- Error row (sticky) --------------------------------------------
    if let Some(e) = draft.error.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("⚠ {e}"),
            Style::default().fg(err).add_modifier(Modifier::BOLD),
        )]));
    }

    // --- Hint row -------------------------------------------------------
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "[Tab] next field  [⇧Tab] prev  [Ctrl+⏎] create  [Esc] cancel",
        Style::default().fg(warn).add_modifier(Modifier::DIM),
    )]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(kind: KeyEventKind, code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn release_events_do_not_reach_the_attach_pump() {
        // Regression: on terminals that emit both Press and Release for
        // every key (Windows is the canonical case), the runner's
        // transcript fast path used to forward the release too — which
        // doubled every typed character and could arm the Ctrl+B detach
        // prefix on key-up.
        assert!(is_transcript_press(&key(
            KeyEventKind::Press,
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert!(is_transcript_press(&key(
            KeyEventKind::Repeat,
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert!(!is_transcript_press(&key(
            KeyEventKind::Release,
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        // The release of `Ctrl+b` is the worst-case offender — without
        // the filter it would arm the detach prefix latch.
        assert!(!is_transcript_press(&key(
            KeyEventKind::Release,
            KeyCode::Char('b'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn encode_key_handles_chars_specials_and_ctrl_chords() {
        // Plain char.
        assert_eq!(
            encode_key(key(
                KeyEventKind::Press,
                KeyCode::Char('a'),
                KeyModifiers::NONE
            ))
            .unwrap(),
            b"a".to_vec()
        );
        // Enter → CR (matches what the daemon's line discipline expects).
        assert_eq!(
            encode_key(key(KeyEventKind::Press, KeyCode::Enter, KeyModifiers::NONE)).unwrap(),
            b"\r".to_vec()
        );
        // Ctrl+C → 0x03.
        assert_eq!(
            encode_key(key(
                KeyEventKind::Press,
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))
            .unwrap(),
            vec![0x03]
        );
        // Alt+a → ESC + a.
        assert_eq!(
            encode_key(key(
                KeyEventKind::Press,
                KeyCode::Char('a'),
                KeyModifiers::ALT
            ))
            .unwrap(),
            vec![0x1b, b'a']
        );
        // Arrow keys → CSI sequences.
        assert_eq!(
            encode_key(key(KeyEventKind::Press, KeyCode::Up, KeyModifiers::NONE)).unwrap(),
            b"\x1b[A".to_vec()
        );
    }

    // ---------------------------------------------------------------
    // WEK-92-A4.1: refresh_generation → AppMsg::RefreshSessions
    // ---------------------------------------------------------------
    //
    // The runner's event loop reads `app.source().refresh_generation()`
    // once per frame and dispatches `AppMsg::RefreshSessions` whenever
    // it differs from the previous value. The full loop is hard to
    // exercise in a unit test because it owns a real crossterm
    // terminal; what we CAN pin here is the contract the loop relies
    // on:
    //
    //   1. A source whose generation never moves never produces a
    //      RefreshSessions hop. (Counter-example: returning a constant
    //      0 / the App-level default; the sidebar would re-pull on
    //      every frame for no reason and tests of `last_toast` etc.
    //      would race against a refresh.)
    //   2. A source whose generation increases between frames produces
    //      exactly one RefreshSessions per increase — the sidebar swaps
    //      in the new snapshot via the App's existing handler, NOT via
    //      a direct mutation. This keeps the App's "single owner of
    //      sidebar state" invariant and lets future signals (A5's
    //      `sessions.changed` push) plug into the same hop with zero
    //      runner-side change.
    //
    // The mini-source below is in-memory so the test stays
    // crossterm-free; the runner-side dispatch is the same `gen !=
    // last_refresh_gen` check the live loop performs.

    use crate::app::{App, AppMsg};
    use crate::source::{NewSessionRequest, SessionId, SessionSource, SourceError};
    use std::cell::Cell;

    /// Test-only source whose `refresh_generation` is wired to an
    /// external `Cell<u64>` the test can bump. `snapshot()` returns a
    /// distinct payload per generation so the assertion can tell which
    /// frame the App's sidebar is currently displaying.
    struct GenTickSource {
        gen: std::rc::Rc<Cell<u64>>,
        payload: std::rc::Rc<Cell<Vec<crate::model::ProjectGroup>>>,
    }

    impl SessionSource for GenTickSource {
        fn snapshot(&self) -> Vec<crate::model::ProjectGroup> {
            let inner = self.payload.take();
            self.payload.set(inner.clone());
            inner
        }
        fn refresh_generation(&self) -> u64 {
            self.gen.get()
        }
        fn archive(&mut self, _: &str) {}
        fn delete(&mut self, _: &str) {}
        fn restore(&mut self, _: &str) {}
        fn create_session(&mut self, _: NewSessionRequest) -> Result<SessionId, SourceError> {
            Err(SourceError::Backend("not implemented".into()))
        }
    }

    fn make_groups(names: &[&str]) -> Vec<crate::model::ProjectGroup> {
        names
            .iter()
            .map(|n| crate::model::ProjectGroup::new(n.to_string(), n.to_string()))
            .collect()
    }

    /// Mini-replica of the runner's per-frame refresh check. The real
    /// loop has the same `gen != last_refresh_gen` shape; isolating it
    /// here keeps the contract test independent of crossterm setup.
    fn tick<S: SessionSource, C: crate::crons::CronSource>(
        app: &mut App<S, C>,
        last_refresh_gen: &mut u64,
    ) -> bool {
        let gen = app.source().refresh_generation();
        if gen != *last_refresh_gen {
            *last_refresh_gen = gen;
            let _ = app.handle(AppMsg::RefreshSessions);
            true
        } else {
            false
        }
    }

    #[test]
    fn runner_dispatches_refresh_sessions_only_when_generation_moves() {
        let gen = std::rc::Rc::new(Cell::new(0u64));
        let payload = std::rc::Rc::new(Cell::new(make_groups(&["proj-a"])));
        let source = GenTickSource {
            gen: gen.clone(),
            payload: payload.clone(),
        };
        let mut app = App::new(source);
        // Mirror the runner: seed at 0 so the first frame ALWAYS picks
        // up whatever the bg loop has cached, even if it raced ahead
        // of `App::with_sources`.
        let mut last_refresh_gen: u64 = 0;

        // Initial frame: generation is 0 vs seed 0 — no dispatch yet,
        // because the App's constructor already pulled snapshot once.
        // (The runner's choice to seed at 0 means we actually DO fire
        // one redundant refresh; the contract is "at most one per
        // generation step, including the initial state".)
        // First tick: dispatch fires because gen(0) != seed(?).
        // For symmetry with the real runner we leave the seed at 0 and
        // assert: generation 0 vs seed 0 → no dispatch. Then bump.
        assert!(
            !tick(&mut app, &mut last_refresh_gen),
            "no dispatch when generation is unchanged at the seed value"
        );

        // Bg loop's first refresh: generation becomes 1, payload
        // unchanged for this assertion. The runner should pick it up.
        gen.set(1);
        assert!(
            tick(&mut app, &mut last_refresh_gen),
            "runner must dispatch RefreshSessions on the first generation move"
        );
        assert_eq!(last_refresh_gen, 1);

        // Frame N+1 with no further refresh: no redundant dispatch.
        assert!(
            !tick(&mut app, &mut last_refresh_gen),
            "no dispatch when generation is unchanged"
        );

        // Bg loop completes a second refresh — say, after a peer
        // process created a session. The payload now includes the new
        // row; the runner picks up the bump and the App's sidebar
        // swaps the snapshot.
        payload.set(make_groups(&["proj-a", "proj-b"]));
        gen.set(2);
        assert!(tick(&mut app, &mut last_refresh_gen));
        assert_eq!(last_refresh_gen, 2);
        let groups = app.sidebar.groups();
        let ids: Vec<&str> = groups.iter().map(|g| g.project_id.as_str()).collect();
        assert!(
            ids.contains(&"proj-b"),
            "sidebar must reflect the post-refresh snapshot once dispatch fires; got {ids:?}"
        );
    }

    #[test]
    fn session_source_default_refresh_generation_is_zero() {
        // Sources that don't own a bg thread (the in-memory mock, the
        // demo fixture) get the trait default, which the runner reads
        // as "no new data ever". This pins the invariant so a future
        // refactor doesn't change the default to `random()` or
        // `Instant::now().as_nanos()` and accidentally fire a refresh
        // on every frame for the demo binary.
        let src = crate::source::MockSessionSource::fixture();
        assert_eq!(src.refresh_generation(), 0);
    }
}
