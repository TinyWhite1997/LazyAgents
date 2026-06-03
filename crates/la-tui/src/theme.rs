//! WEK-42 / M4.3 — three-palette theme + key-hint mode + compact layout.
//!
//! Architecture §11.1 reserves a `[ui]` section in `config.toml` with two
//! initial keys (`theme`, `key_hints`). This module owns the *semantics*
//! of those keys plus the additional `compact` switch the M4.3 task adds:
//!
//! - [`Theme::Auto`] defers to the host terminal. We do NOT inspect the
//!   terminal's actual background — `crossterm` cannot read it reliably
//!   on every platform — so Auto inherits ratatui's default style
//!   (`Color::Reset`) and lets the user's terminal supply the canvas. The
//!   only override we apply in Auto mode is the WCAG-AA palette for
//!   accent foregrounds, which `Color::Reset` does not provide.
//! - [`Theme::Dark`] / [`Theme::Light`] paint a fixed accent palette that
//!   has been hand-tuned to clear WCAG 2.1 AA contrast (≥4.5:1 for body
//!   text) against the canonical dark (#0e1116) and light (#ffffff)
//!   backgrounds. Verified by [`tests::wcag_aa_passes_for_both_palettes`].
//!
//! The accent → ratatui `Color` mapping is defined here once; renderers
//! (`runner`, `sidebar`, `status`, `key_hints`) call into [`Palette`]
//! instead of hard-coding `Color::Cyan` / `Color::Green` etc. so a future
//! theme change touches only this file.
//!
//! Why no truecolor? Some terminals (Windows Conhost legacy, low-color
//! TTYs) ignore `Color::Rgb`. We keep both an RGB token (for contrast
//! computation) AND a fallback `Color` variant; renderers receive the
//! `Color` value and trust ratatui to downgrade gracefully.

use ratatui::style::Color;

/// Which colour scheme to render. Maps 1:1 to the `[ui].theme` TOML key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Theme {
    /// Defer to the terminal's default colours. The accent palette still
    /// applies — only the canvas (Background / Foreground / DIM body) is
    /// left to the terminal.
    Auto,
    Dark,
    Light,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::Auto, Theme::Dark, Theme::Light];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Auto => "auto",
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Theme::Auto),
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            _ => None,
        }
    }

    /// Cycle Auto → Dark → Light → Auto. Used by the `T` keybinding.
    pub fn next(self) -> Self {
        match self {
            Theme::Auto => Theme::Dark,
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Auto,
        }
    }
}

/// How aggressively to render the bottom hint bar. Maps 1:1 to the
/// `[ui].key_hints` TOML key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyHintsMode {
    /// Full hint row: every contextual key + globals + meta.
    Rich,
    /// One-line summary: keep `Primary` + at most one other action; drop
    /// teaching keys. Forces the runner to merge the hint row into the
    /// status bar so the screen reclaims a vertical line.
    Compact,
    /// Hide the hint bar entirely. `?` still opens the full overlay.
    Hidden,
}

impl KeyHintsMode {
    pub const ALL: [KeyHintsMode; 3] = [
        KeyHintsMode::Rich,
        KeyHintsMode::Compact,
        KeyHintsMode::Hidden,
    ];

    pub fn label(self) -> &'static str {
        match self {
            KeyHintsMode::Rich => "rich",
            KeyHintsMode::Compact => "compact",
            KeyHintsMode::Hidden => "hidden",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rich" => Some(KeyHintsMode::Rich),
            "compact" => Some(KeyHintsMode::Compact),
            "hidden" => Some(KeyHintsMode::Hidden),
            _ => None,
        }
    }

    /// Cycle Rich → Compact → Hidden → Rich. Used by the `H` keybinding.
    pub fn next(self) -> Self {
        match self {
            KeyHintsMode::Rich => KeyHintsMode::Compact,
            KeyHintsMode::Compact => KeyHintsMode::Hidden,
            KeyHintsMode::Hidden => KeyHintsMode::Rich,
        }
    }
}

/// The named accent slots. Renderers ask the palette for `Accent::Ok`
/// instead of `Color::Green` so the palette can substitute a WCAG-safe
/// shade per theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accent {
    /// Cursor / focus borders / "next action" highlight.
    Primary,
    /// Success / running / enabled.
    Ok,
    /// Warning / pending confirm.
    Warn,
    /// Error / red borders / `⚠` badges.
    Error,
    /// Body text / labels. Renderer leans on the terminal's default fg
    /// when the theme is Auto so this returns `Color::Reset` there.
    Body,
    /// Dim secondary text (placeholders, separators).
    Muted,
    /// Inverted-on-accent text (the dark text drawn on a coloured chip).
    OnAccent,
}

/// Resolved colour table for the active theme.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    theme: Theme,
    primary: Color,
    ok: Color,
    warn: Color,
    error: Color,
    body: Color,
    muted: Color,
    on_accent: Color,
}

impl Palette {
    pub fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Auto => Palette {
                theme,
                // Auto inherits the terminal's body / muted; accents
                // still come from the dark-tuned palette because WCAG
                // accent contrast against an unknown background is
                // better served by saturated colours than the named
                // ANSI variants ratatui falls back to.
                primary: dark_primary(),
                ok: dark_ok(),
                warn: dark_warn(),
                error: dark_error(),
                body: Color::Reset,
                muted: Color::DarkGray,
                on_accent: Color::Black,
            },
            Theme::Dark => Palette {
                theme,
                primary: dark_primary(),
                ok: dark_ok(),
                warn: dark_warn(),
                error: dark_error(),
                body: Color::Rgb(0xe6, 0xed, 0xf3),
                muted: Color::Rgb(0x7d, 0x86, 0x90),
                on_accent: Color::Black,
            },
            Theme::Light => Palette {
                theme,
                primary: light_primary(),
                ok: light_ok(),
                warn: light_warn(),
                error: light_error(),
                body: Color::Rgb(0x1f, 0x23, 0x28),
                muted: Color::Rgb(0x57, 0x60, 0x6a),
                on_accent: Color::White,
            },
        }
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn color(&self, slot: Accent) -> Color {
        match slot {
            Accent::Primary => self.primary,
            Accent::Ok => self.ok,
            Accent::Warn => self.warn,
            Accent::Error => self.error,
            Accent::Body => self.body,
            Accent::Muted => self.muted,
            Accent::OnAccent => self.on_accent,
        }
    }
}

// Hand-tuned hex values; chosen so contrast against the canonical dark
// background (#0e1116) and light background (#ffffff) clears WCAG-AA
// (≥4.5:1) for normal body text. See `tests::wcag_aa_passes_for_both_palettes`.
fn dark_primary() -> Color {
    // Cyan-ish: keeps muscle-memory parity with the pre-M4.3 `Color::Cyan`
    // accents while bumping luminance for AA.
    Color::Rgb(0x6c, 0xc0, 0xdf)
}
fn dark_ok() -> Color {
    Color::Rgb(0x7c, 0xe3, 0x8a)
}
fn dark_warn() -> Color {
    Color::Rgb(0xff, 0xc8, 0x57)
}
fn dark_error() -> Color {
    Color::Rgb(0xff, 0x7b, 0x72)
}
fn light_primary() -> Color {
    Color::Rgb(0x05, 0x59, 0x80)
}
fn light_ok() -> Color {
    Color::Rgb(0x1a, 0x73, 0x2e)
}
fn light_warn() -> Color {
    Color::Rgb(0x8a, 0x5a, 0x00)
}
fn light_error() -> Color {
    Color::Rgb(0xb3, 0x10, 0x10)
}

/// Backgrounds used for the contrast self-check. Real terminals will
/// override these; the values exist to give the WCAG test a concrete
/// canvas.
#[cfg(test)]
const DARK_BG: (u8, u8, u8) = (0x0e, 0x11, 0x16);
#[cfg(test)]
const LIGHT_BG: (u8, u8, u8) = (0xff, 0xff, 0xff);

/// Compute the relative luminance of an sRGB triple per WCAG 2.x §1.4.3.
#[cfg(test)]
fn relative_luminance(rgb: (u8, u8, u8)) -> f64 {
    fn linearize(c: u8) -> f64 {
        let cs = c as f64 / 255.0;
        if cs <= 0.039_28 {
            cs / 12.92
        } else {
            ((cs + 0.055) / 1.055).powf(2.4)
        }
    }
    let r = linearize(rgb.0);
    let g = linearize(rgb.1);
    let b = linearize(rgb.2);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Contrast ratio per WCAG 2.x. Returns ≥1.0; AA body text wants ≥4.5.
#[cfg(test)]
fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (lighter, darker) = if la > lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Test-only helper: extract the RGB triple from a `Color::Rgb`. Returns
/// `None` for named ANSI colours (which we do NOT contrast-check — they
/// are the terminal's responsibility).
#[cfg(test)]
fn rgb_of(c: Color) -> Option<(u8, u8, u8)> {
    if let Color::Rgb(r, g, b) = c {
        Some((r, g, b))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_cycle_round_trips() {
        let mut t = Theme::Auto;
        for _ in 0..Theme::ALL.len() {
            t = t.next();
        }
        assert_eq!(t, Theme::Auto, "next() must cycle through ALL");
    }

    #[test]
    fn key_hints_cycle_round_trips() {
        let mut h = KeyHintsMode::Rich;
        for _ in 0..KeyHintsMode::ALL.len() {
            h = h.next();
        }
        assert_eq!(h, KeyHintsMode::Rich);
    }

    #[test]
    fn label_roundtrip_is_lossless() {
        for t in Theme::ALL {
            assert_eq!(Theme::from_label(t.label()), Some(t));
        }
        for h in KeyHintsMode::ALL {
            assert_eq!(KeyHintsMode::from_label(h.label()), Some(h));
        }
        assert_eq!(Theme::from_label("DARK"), Some(Theme::Dark));
        assert_eq!(Theme::from_label("nope"), None);
    }

    /// Acceptance: 颜色满足 WCAG AA 对比度 (≥4.5:1) for body-class text.
    /// Every Rgb accent in the dark palette must clear AA against the
    /// dark canvas; the light palette must clear AA against the light
    /// canvas.
    #[test]
    fn wcag_aa_passes_for_both_palettes() {
        let cases: [(Theme, (u8, u8, u8), &[Accent]); 2] = [
            (
                Theme::Dark,
                DARK_BG,
                &[
                    Accent::Primary,
                    Accent::Ok,
                    Accent::Warn,
                    Accent::Error,
                    Accent::Body,
                ],
            ),
            (
                Theme::Light,
                LIGHT_BG,
                &[
                    Accent::Primary,
                    Accent::Ok,
                    Accent::Warn,
                    Accent::Error,
                    Accent::Body,
                ],
            ),
        ];
        for (theme, bg, slots) in cases {
            let pal = Palette::for_theme(theme);
            for slot in slots {
                let c = pal.color(*slot);
                let rgb = rgb_of(c).unwrap_or_else(|| {
                    panic!("palette accent {slot:?} in {theme:?} must be Rgb for AA check")
                });
                let ratio = contrast_ratio(rgb, bg);
                assert!(
                    ratio >= 4.5,
                    "{theme:?}::{slot:?} contrast {ratio:.2}:1 < AA 4.5:1 (fg #{:02x}{:02x}{:02x} on bg #{:02x}{:02x}{:02x})",
                    rgb.0, rgb.1, rgb.2, bg.0, bg.1, bg.2,
                );
            }
        }
    }

    /// Auto theme inherits `Color::Reset` for body — that's expected, and
    /// we intentionally do NOT contrast-check it (the canvas is the
    /// terminal's).
    #[test]
    fn auto_palette_leaves_body_unset() {
        let pal = Palette::for_theme(Theme::Auto);
        assert_eq!(pal.color(Accent::Body), Color::Reset);
    }
}
