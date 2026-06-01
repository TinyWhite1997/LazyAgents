//! [`vte::Perform`] adapter that folds PTY bytes into a line-oriented
//! transcript suitable for ratatui rendering.
//!
//! ## Why a custom Perform instead of a full terminal emulator?
//!
//! The conversation main area is a *log view*, not a full VT100 surface:
//! the user reads agent output and scrolls back through history. A complete
//! emulator (alacritty-grid, fixed cells, cursor addressing, alternate
//! screen) would be wasted work and would actively misrepresent multi-line
//! agent prose, which has no fixed column width.
//!
//! So this module keeps the model intentionally narrow:
//!
//! - print bytes append to the current pending line.
//! - `\n` commits the pending line into the history vector.
//! - `\r` resets the write column to 0 (subsequent prints overwrite — used by
//!   progress bars and spinners).
//! - `\t` expands to spaces (8-column stops, the de-facto terminal default).
//! - `BS` deletes the cell to the left of the cursor.
//! - SGR (CSI `m`) updates the current style and is the only escape
//!   sequence we *interpret*. Everything else — CSI cursor moves, OSC window
//!   titles, DCS, ConPTY's `CSI 6 n` cursor query (architecture §6.5) — is
//!   absorbed silently so it never leaks bytes into the transcript.

use ratatui::style::{Color, Modifier, Style};
use vte::{Params, Perform};

/// One rendered cell: a character plus its style at the moment it was
/// printed. Width is computed lazily by the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledCell {
    pub ch: char,
    pub style: Style,
}

/// A single line of committed (or in-flight) transcript output.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalLine {
    pub cells: Vec<StyledCell>,
}

impl TerminalLine {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn to_plain_string(&self) -> String {
        self.cells.iter().map(|c| c.ch).collect()
    }
}

/// Tab stop every 8 columns — matches `xterm` and most terminals.
const TAB_WIDTH: usize = 8;

/// Transcript model fed by [`vte::Parser`].
///
/// Holds:
/// - a vector of committed lines (oldest first),
/// - one pending line currently being written,
/// - the write column inside the pending line,
/// - the active SGR style.
///
/// The caller (a [`crate::transcript::Transcript`]) calls
/// [`TerminalScreen::feed`] with raw PTY bytes, then reads
/// [`TerminalScreen::take_committed`] to drain newly-finished lines into the
/// scrollback ring, and [`TerminalScreen::pending`] for the in-flight tail.
pub struct TerminalScreen {
    committed: Vec<TerminalLine>,
    pending: TerminalLine,
    /// Column the next [`Perform::print`] will write into. May exceed
    /// `pending.cells.len()` after a long-jump CR; new prints pad with
    /// spaces in [`Self::write_char`].
    col: usize,
    style: Style,
    parser: vte::Parser,
}

impl Default for TerminalScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalScreen {
    pub fn new() -> Self {
        Self {
            committed: Vec::new(),
            pending: TerminalLine::default(),
            col: 0,
            style: Style::default(),
            parser: vte::Parser::new(),
        }
    }

    /// Drive the parser over `bytes`. Safe to call with partial frames —
    /// vte's state machine is byte-incremental.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Run the parser against a borrowed performer so we can keep the
        // mutable buffers on `self`. vte requires &mut to both sides.
        let mut sink = PerformSink {
            committed: &mut self.committed,
            pending: &mut self.pending,
            col: &mut self.col,
            style: &mut self.style,
        };
        self.parser.advance(&mut sink, bytes);
    }

    /// Drain finished lines into the caller. After this call,
    /// `committed` is empty and only the pending tail remains.
    pub fn take_committed(&mut self) -> Vec<TerminalLine> {
        std::mem::take(&mut self.committed)
    }

    /// Reference to the in-flight (uncommitted) line. Renderers display this
    /// as the live tail so the user sees output before it's terminated by
    /// `\n`.
    pub fn pending(&self) -> &TerminalLine {
        &self.pending
    }
}

/// Borrowed mutable view used while [`vte::Parser::advance`] runs.
struct PerformSink<'a> {
    committed: &'a mut Vec<TerminalLine>,
    pending: &'a mut TerminalLine,
    col: &'a mut usize,
    style: &'a mut Style,
}

impl<'a> PerformSink<'a> {
    fn write_char(&mut self, ch: char) {
        // Pad with spaces if a previous CR left col past the end of cells.
        while self.pending.cells.len() < *self.col {
            self.pending.cells.push(StyledCell {
                ch: ' ',
                style: Style::default(),
            });
        }
        let cell = StyledCell {
            ch,
            style: *self.style,
        };
        if *self.col < self.pending.cells.len() {
            self.pending.cells[*self.col] = cell;
        } else {
            self.pending.cells.push(cell);
        }
        *self.col += 1;
    }

    fn newline(&mut self) {
        let line = std::mem::take(self.pending);
        self.committed.push(line);
        *self.col = 0;
    }

    fn carriage_return(&mut self) {
        *self.col = 0;
    }

    fn backspace(&mut self) {
        if *self.col > 0 {
            *self.col -= 1;
        }
    }

    fn tab(&mut self) {
        let target = (*self.col / TAB_WIDTH + 1) * TAB_WIDTH;
        while *self.col < target {
            self.write_char(' ');
        }
    }

    /// Apply one Select Graphic Rendition (SGR) parameter run to `style`.
    /// Resets, attribute toggles, 16-color FG/BG, 256-color (`38;5;n` /
    /// `48;5;n`), and truecolor (`38;2;r;g;b` / `48;2;r;g;b`) are honored;
    /// anything unrecognised is silently ignored so we never crash on an
    /// extended sequence.
    fn apply_sgr(&mut self, params: &Params) {
        // Flatten parameters into a flat Vec for easier streaming. Each
        // logical parameter can have multiple sub-params separated by `:`,
        // but for SGR we treat both `;` and `:` as the same separator —
        // this matches what xterm does and what most clients emit.
        let mut flat: Vec<u16> = Vec::new();
        for group in params.iter() {
            for &v in group {
                flat.push(v);
            }
        }
        if flat.is_empty() {
            *self.style = Style::default();
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            let p = flat[i];
            match p {
                0 => *self.style = Style::default(),
                1 => *self.style = self.style.add_modifier(Modifier::BOLD),
                2 => *self.style = self.style.add_modifier(Modifier::DIM),
                3 => *self.style = self.style.add_modifier(Modifier::ITALIC),
                4 => *self.style = self.style.add_modifier(Modifier::UNDERLINED),
                7 => *self.style = self.style.add_modifier(Modifier::REVERSED),
                9 => *self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
                22 => *self.style = self.style.remove_modifier(Modifier::BOLD | Modifier::DIM),
                23 => *self.style = self.style.remove_modifier(Modifier::ITALIC),
                24 => *self.style = self.style.remove_modifier(Modifier::UNDERLINED),
                27 => *self.style = self.style.remove_modifier(Modifier::REVERSED),
                29 => *self.style = self.style.remove_modifier(Modifier::CROSSED_OUT),
                30..=37 => *self.style = self.style.fg(ansi_color((p - 30) as u8)),
                39 => {
                    let mut s = *self.style;
                    s.fg = None;
                    *self.style = s;
                }
                40..=47 => *self.style = self.style.bg(ansi_color((p - 40) as u8)),
                49 => {
                    let mut s = *self.style;
                    s.bg = None;
                    *self.style = s;
                }
                90..=97 => *self.style = self.style.fg(ansi_bright((p - 90) as u8)),
                100..=107 => *self.style = self.style.bg(ansi_bright((p - 100) as u8)),
                38 | 48 => {
                    // Extended color: `5;n` (256-color) or `2;r;g;b` (RGB).
                    let is_fg = p == 38;
                    if let Some(&kind) = flat.get(i + 1) {
                        match kind {
                            5 => {
                                if let Some(&idx) = flat.get(i + 2) {
                                    let c = Color::Indexed(idx as u8);
                                    *self.style = if is_fg {
                                        self.style.fg(c)
                                    } else {
                                        self.style.bg(c)
                                    };
                                    i += 2;
                                }
                            }
                            2 => {
                                if let (Some(&r), Some(&g), Some(&b)) =
                                    (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4))
                                {
                                    let c = Color::Rgb(r as u8, g as u8, b as u8);
                                    *self.style = if is_fg {
                                        self.style.fg(c)
                                    } else {
                                        self.style.bg(c)
                                    };
                                    i += 4;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {} // ignore unrecognised SGR params
            }
            i += 1;
        }
    }
}

fn ansi_color(n: u8) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

fn ansi_bright(n: u8) -> Color {
    match n {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

impl<'a> Perform for PerformSink<'a> {
    fn print(&mut self, c: char) {
        self.write_char(c);
    }

    fn execute(&mut self, byte: u8) {
        // C0/C1 controls. We intentionally handle only the ones a transcript
        // model needs; bell / shift in / shift out / etc. are absorbed.
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.carriage_return(),
            b'\x08' => self.backspace(),
            b'\t' => self.tab(),
            // 0x0B (VT), 0x0C (FF) — treat as line feed: rare but real.
            // 0x84 (IND), 0x85 (NEL) — C1 line break controls. ConPTY does
            // not emit these but agents sometimes do; treat as LF so the
            // line break is preserved instead of vanishing.
            0x0B | 0x0C | 0x84 | 0x85 => self.newline(),
            _ => {} // bell, SI, SO, NUL, ...
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // DCS open — swallowed. `put` accumulates payload, `unhook` ends.
    }

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC sequences: window title (OSC 0/2), hyperlinks (OSC 8), iTerm
        // image protocol, etc. Absorbed silently — ConPTY emits OSC chatter
        // on session start (architecture §6.5).
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // CSI sequences. We interpret SGR (`m`) and silently swallow
        // everything else: cursor movement, erase, scroll, DSR. The
        // critical case from architecture §6.5 is `CSI 6 n` (cursor
        // position report) injected by ConPTY — by ignoring it here, no
        // raw bytes leak into the transcript.
        if action == 'm' {
            self.apply_sgr(params);
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // ESC sequences (charset switches, RIS, etc.) — absorbed.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_lines(screen: &mut TerminalScreen) -> Vec<String> {
        let mut out: Vec<String> = screen
            .take_committed()
            .iter()
            .map(|l| l.to_plain_string())
            .collect();
        if !screen.pending().is_empty() {
            out.push(screen.pending().to_plain_string());
        }
        out
    }

    #[test]
    fn prints_plain_text() {
        let mut s = TerminalScreen::new();
        s.feed(b"hello world\n");
        assert_eq!(plain_lines(&mut s), vec!["hello world".to_string()]);
    }

    #[test]
    fn handles_multiple_lines() {
        let mut s = TerminalScreen::new();
        s.feed(b"line one\nline two\nline three");
        assert_eq!(
            plain_lines(&mut s),
            vec!["line one", "line two", "line three"]
        );
    }

    #[test]
    fn carriage_return_overwrites() {
        // Progress bar pattern: write "loading...", CR, then overwrite.
        let mut s = TerminalScreen::new();
        s.feed(b"loading\rDONE!\n");
        assert_eq!(plain_lines(&mut s), vec!["DONE!ng".to_string()]);
    }

    #[test]
    fn backspace_moves_cursor_only() {
        let mut s = TerminalScreen::new();
        s.feed(b"abcd\x08\x08XY\n");
        assert_eq!(plain_lines(&mut s), vec!["abXY".to_string()]);
    }

    #[test]
    fn tab_pads_to_8_columns() {
        let mut s = TerminalScreen::new();
        s.feed(b"a\tb\n");
        assert_eq!(plain_lines(&mut s), vec!["a       b".to_string()]);
    }

    #[test]
    fn ignores_cursor_query_csi_6n() {
        // ConPTY injects this on attach. Architecture §6.5: must NOT leak
        // bytes into the transcript.
        let mut s = TerminalScreen::new();
        s.feed(b"before\x1b[6nafter\n");
        assert_eq!(plain_lines(&mut s), vec!["beforeafter".to_string()]);
    }

    #[test]
    fn ignores_cursor_move_and_erase_sequences() {
        // ConPTY's typical attach burst: cursor home, clear screen, query.
        let mut s = TerminalScreen::new();
        s.feed(b"\x1b[H\x1b[2J\x1b[6n");
        s.feed(b"hello\n");
        assert_eq!(plain_lines(&mut s), vec!["hello".to_string()]);
    }

    #[test]
    fn ignores_osc_window_title() {
        // OSC 0 ; "title" BEL — common from agents announcing themselves.
        let mut s = TerminalScreen::new();
        s.feed(b"\x1b]0;agent title\x07hi\n");
        assert_eq!(plain_lines(&mut s), vec!["hi".to_string()]);
    }

    #[test]
    fn ignores_osc_terminated_by_st() {
        // ST = ESC \\ (0x1b 0x5c).
        let mut s = TerminalScreen::new();
        s.feed(b"\x1b]8;;https://x/\x1b\\link\x1b]8;;\x1b\\\n");
        assert_eq!(plain_lines(&mut s), vec!["link".to_string()]);
    }

    #[test]
    fn sgr_color_applied_to_cells() {
        let mut s = TerminalScreen::new();
        s.feed(b"\x1b[31mred\x1b[0m\n");
        let lines = s.take_committed();
        let red_cells = &lines[0].cells;
        assert_eq!(red_cells[0].ch, 'r');
        assert_eq!(red_cells[0].style.fg, Some(Color::Red));
        // After reset the style is default, so any subsequent print would
        // carry no fg — we have no post-reset cell here, just check that
        // the reset itself didn't crash.
    }

    #[test]
    fn sgr_truecolor() {
        let mut s = TerminalScreen::new();
        s.feed(b"\x1b[38;2;10;20;30mx\n");
        let lines = s.take_committed();
        assert_eq!(lines[0].cells[0].style.fg, Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn split_escape_across_feeds_still_parses() {
        // Bytes can arrive in arbitrary chunks. The parser keeps state, so
        // splitting CSI 6 n down the middle must still swallow it.
        let mut s = TerminalScreen::new();
        s.feed(b"a\x1b");
        s.feed(b"[6");
        s.feed(b"nb\n");
        assert_eq!(plain_lines(&mut s), vec!["ab".to_string()]);
    }
}
