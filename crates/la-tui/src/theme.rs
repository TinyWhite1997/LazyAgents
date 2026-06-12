//! WEK-42 / M4.3 — themeable palette + key-hint mode + compact layout.
//!
//! Architecture §11.1 reserves a `[ui]` section in `config.toml` with two
//! initial keys (`theme`, `key_hints`). This module owns the *semantics*
//! of those keys plus the `compact` switch the M4.3 task added.
//!
//! ## Theme model (multi-theme rework)
//!
//! The original three-variant `Theme` enum (auto / dark / light) was
//! replaced by a string-id [`ThemeCatalog`] so we can ship a wide set of
//! popular open-source palettes (Catppuccin, Gruvbox, Nord, Dracula,
//! Tokyo Night, Solarized) *and* let users define their own palettes in
//! `config.toml` ([`crate::ui_prefs`] parses `[[ui.custom_theme]]`).
//!
//! - `[ui].theme` is now a free string — a theme **id** (`"catppuccin-mocha"`).
//! - [`ThemeCatalog::palette`] resolves an id to a [`Palette`], falling
//!   back to the `auto` palette on an unknown id so a typo demotes
//!   gracefully instead of crashing.
//! - `auto` keeps the pre-rework behaviour: it does NOT paint a canvas
//!   (`bg`/`body` are [`Color::Reset`]) and inherits the terminal's
//!   default background. Every *named* theme paints a full canvas
//!   (background + foreground) so Dracula / Nord / etc. look authentic.
//!
//! The accent → ratatui `Color` mapping lives here once; renderers
//! (`runner`, `sidebar`, `status`, `key_hints`) ask [`Palette`] for an
//! [`Accent`] slot instead of hard-coding `Color::Cyan` / `Color::Green`
//! so a theme change touches only this file.
//!
//! Why no truecolor concern? Some terminals (Windows Conhost legacy,
//! low-color TTYs) ignore `Color::Rgb`. Renderers receive the `Color`
//! value and trust ratatui to downgrade gracefully.
//!
//! Contrast: only the built-in `dark` / `light` palettes are asserted to
//! clear WCAG-AA (see `tests::wcag_aa_passes_for_builtin_dark_light`).
//! Third-party and user palettes are trusted as-authored.

use ratatui::style::Color;

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

/// The default theme id — the `auto` palette, which defers the canvas to
/// the host terminal.
pub const DEFAULT_THEME_ID: &str = "auto";

/// The named accent slots. Renderers ask the palette for `Accent::Ok`
/// instead of `Color::Green` so the palette can substitute a theme-safe
/// shade.
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
    /// Body text / labels. Leans on the terminal default fg in the `auto`
    /// theme (`Color::Reset`).
    Body,
    /// Dim secondary text (placeholders, separators).
    Muted,
    /// Inverted-on-accent text (the dark text drawn on a coloured chip).
    OnAccent,
    /// The window canvas background. `Color::Reset` for `auto` (terminal
    /// supplies it); a concrete colour for every named theme.
    Background,
}

/// Resolved colour table for one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    primary: Color,
    ok: Color,
    warn: Color,
    error: Color,
    body: Color,
    muted: Color,
    on_accent: Color,
    bg: Color,
}

impl Palette {
    pub fn color(&self, slot: Accent) -> Color {
        match slot {
            Accent::Primary => self.primary,
            Accent::Ok => self.ok,
            Accent::Warn => self.warn,
            Accent::Error => self.error,
            Accent::Body => self.body,
            Accent::Muted => self.muted,
            Accent::OnAccent => self.on_accent,
            Accent::Background => self.bg,
        }
    }

    /// The canvas background. `Color::Reset` means "leave it to the
    /// terminal" — renderers skip the full-frame fill in that case.
    pub fn bg(&self) -> Color {
        self.bg
    }

    /// The `auto` palette: saturated accents, no painted canvas. Handy
    /// for default/standalone renders (status bar, sidebar) and tests
    /// that don't carry a [`ThemeCatalog`].
    pub fn auto() -> Palette {
        auto_palette()
    }

    /// Build a fully-specified palette from sRGB triples. Used by
    /// [`crate::ui_prefs`] to materialise user-defined `[[ui.custom_theme]]`
    /// entries. Every slot is concrete (no `Color::Reset`), so a custom
    /// theme always paints its own canvas.
    #[allow(clippy::too_many_arguments)]
    pub fn from_rgb(
        bg: (u8, u8, u8),
        body: (u8, u8, u8),
        muted: (u8, u8, u8),
        primary: (u8, u8, u8),
        ok: (u8, u8, u8),
        warn: (u8, u8, u8),
        error: (u8, u8, u8),
        on_accent: (u8, u8, u8),
    ) -> Palette {
        rgb_palette(bg, body, muted, primary, ok, warn, error, on_accent)
    }
}

/// A named theme: a stable `id` (the `[ui].theme` value), a human label
/// for the picker, and its resolved [`Palette`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeSpec {
    pub id: String,
    pub label: String,
    pub palette: Palette,
}

/// The ordered set of selectable themes: built-ins first, then any
/// user-defined custom themes. A custom theme whose `id` matches a
/// built-in overrides it in place (keeping list position stable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeCatalog {
    themes: Vec<ThemeSpec>,
}

impl ThemeCatalog {
    /// Built-ins only.
    pub fn builtin() -> Self {
        Self {
            themes: builtin_specs(),
        }
    }

    /// Built-ins plus user-defined custom themes. A custom theme whose id
    /// collides with a built-in replaces the built-in (so a user can
    /// retune `dark` without losing its slot); otherwise it is appended.
    pub fn with_custom(custom: Vec<ThemeSpec>) -> Self {
        let mut themes = builtin_specs();
        for c in custom {
            if let Some(existing) = themes.iter_mut().find(|t| t.id == c.id) {
                *existing = c;
            } else {
                themes.push(c);
            }
        }
        Self { themes }
    }

    /// Resolve a theme id to its palette, falling back to the `auto`
    /// palette when the id is unknown (graceful demotion on a typo).
    pub fn palette(&self, id: &str) -> Palette {
        self.themes
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.palette)
            .unwrap_or_else(auto_palette)
    }

    /// True when a theme with this id exists in the catalog.
    pub fn contains(&self, id: &str) -> bool {
        self.themes.iter().any(|t| t.id == id)
    }

    /// All theme ids in display order.
    pub fn ids(&self) -> Vec<String> {
        self.themes.iter().map(|t| t.id.clone()).collect()
    }

    /// All theme specs in display order.
    pub fn specs(&self) -> &[ThemeSpec] {
        &self.themes
    }

    /// The display label for an id, or the id itself if unknown.
    pub fn label_for(&self, id: &str) -> String {
        self.themes
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.label.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

impl Default for ThemeCatalog {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Construct a [`Palette`] from raw sRGB tuples. `bg`/`body` as
/// `Color::Reset` is expressed by [`auto_palette`] directly.
#[allow(clippy::too_many_arguments)]
const fn rgb_palette(
    bg: (u8, u8, u8),
    body: (u8, u8, u8),
    muted: (u8, u8, u8),
    primary: (u8, u8, u8),
    ok: (u8, u8, u8),
    warn: (u8, u8, u8),
    error: (u8, u8, u8),
    on_accent: (u8, u8, u8),
) -> Palette {
    Palette {
        primary: Color::Rgb(primary.0, primary.1, primary.2),
        ok: Color::Rgb(ok.0, ok.1, ok.2),
        warn: Color::Rgb(warn.0, warn.1, warn.2),
        error: Color::Rgb(error.0, error.1, error.2),
        body: Color::Rgb(body.0, body.1, body.2),
        muted: Color::Rgb(muted.0, muted.1, muted.2),
        on_accent: Color::Rgb(on_accent.0, on_accent.1, on_accent.2),
        bg: Color::Rgb(bg.0, bg.1, bg.2),
    }
}

/// The `auto` palette: no canvas (terminal supplies bg + body), but
/// saturated accents so contrast against an unknown background is
/// reasonable. Matches the pre-rework `Theme::Auto` behaviour.
fn auto_palette() -> Palette {
    Palette {
        primary: Color::Rgb(0x6c, 0xc0, 0xdf),
        ok: Color::Rgb(0x7c, 0xe3, 0x8a),
        warn: Color::Rgb(0xff, 0xc8, 0x57),
        error: Color::Rgb(0xff, 0x7b, 0x72),
        body: Color::Reset,
        muted: Color::DarkGray,
        on_accent: Color::Black,
        bg: Color::Reset,
    }
}

/// The canonical built-in theme set, in display order.
pub fn builtin_specs() -> Vec<ThemeSpec> {
    fn spec(id: &str, label: &str, palette: Palette) -> ThemeSpec {
        ThemeSpec {
            id: id.to_string(),
            label: label.to_string(),
            palette,
        }
    }

    vec![
        spec("auto", "Auto (terminal)", auto_palette()),
        // Pre-rework `dark`: the same accent hexes, now with the canvas
        // it was implicitly drawn on (#0e1116) painted explicitly.
        spec(
            "dark",
            "Dark",
            Palette {
                primary: Color::Rgb(0x6c, 0xc0, 0xdf),
                ok: Color::Rgb(0x7c, 0xe3, 0x8a),
                warn: Color::Rgb(0xff, 0xc8, 0x57),
                error: Color::Rgb(0xff, 0x7b, 0x72),
                body: Color::Rgb(0xe6, 0xed, 0xf3),
                muted: Color::Rgb(0x7d, 0x86, 0x90),
                on_accent: Color::Black,
                bg: Color::Rgb(0x0e, 0x11, 0x16),
            },
        ),
        spec(
            "light",
            "Light",
            Palette {
                primary: Color::Rgb(0x05, 0x59, 0x80),
                ok: Color::Rgb(0x1a, 0x73, 0x2e),
                warn: Color::Rgb(0x8a, 0x5a, 0x00),
                error: Color::Rgb(0xb3, 0x10, 0x10),
                body: Color::Rgb(0x1f, 0x23, 0x28),
                muted: Color::Rgb(0x57, 0x60, 0x6a),
                on_accent: Color::White,
                bg: Color::Rgb(0xff, 0xff, 0xff),
            },
        ),
        // --- Catppuccin (https://catppuccin.com/palette) ----------------
        // Latte (light). bg=Base, body=Text, muted=Subtext0,
        // primary=Blue, ok=Green, warn=Yellow(→darker for contrast? keep
        // Peach), error=Red.
        spec(
            "catppuccin-latte",
            "Catppuccin Latte",
            rgb_palette(
                (0xef, 0xf1, 0xf5), // base
                (0x4c, 0x4f, 0x69), // text
                (0x6c, 0x6f, 0x85), // subtext0
                (0x1e, 0x66, 0xf5), // blue
                (0x40, 0xa0, 0x2b), // green
                (0xdf, 0x8e, 0x1d), // yellow
                (0xd2, 0x0f, 0x39), // red
                (0xef, 0xf1, 0xf5), // on-accent ≈ base (dark text on light chip)
            ),
        ),
        spec(
            "catppuccin-frappe",
            "Catppuccin Frappé",
            rgb_palette(
                (0x30, 0x34, 0x46), // base
                (0xc6, 0xd0, 0xf5), // text
                (0xa5, 0xad, 0xce), // subtext0
                (0x8c, 0xaa, 0xee), // blue
                (0xa6, 0xd1, 0x89), // green
                (0xe5, 0xc8, 0x90), // yellow
                (0xe7, 0x82, 0x84), // red
                (0x30, 0x34, 0x46), // on-accent ≈ base
            ),
        ),
        spec(
            "catppuccin-macchiato",
            "Catppuccin Macchiato",
            rgb_palette(
                (0x24, 0x27, 0x3a), // base
                (0xca, 0xd3, 0xf5), // text
                (0xa5, 0xad, 0xcb), // subtext0
                (0x8a, 0xad, 0xf4), // blue
                (0xa6, 0xda, 0x95), // green
                (0xee, 0xd4, 0x9f), // yellow
                (0xed, 0x87, 0x96), // red
                (0x24, 0x27, 0x3a), // on-accent ≈ base
            ),
        ),
        spec(
            "catppuccin-mocha",
            "Catppuccin Mocha",
            rgb_palette(
                (0x1e, 0x1e, 0x2e), // base
                (0xcd, 0xd6, 0xf4), // text
                (0xa6, 0xad, 0xc8), // subtext0
                (0x89, 0xb4, 0xfa), // blue
                (0xa6, 0xe3, 0xa1), // green
                (0xf9, 0xe2, 0xaf), // yellow
                (0xf3, 0x8b, 0xa8), // red
                (0x1e, 0x1e, 0x2e), // on-accent ≈ base
            ),
        ),
        // --- Gruvbox (https://github.com/morhetz/gruvbox) ---------------
        spec(
            "gruvbox-dark",
            "Gruvbox Dark",
            rgb_palette(
                (0x28, 0x28, 0x28), // bg
                (0xeb, 0xdb, 0xb2), // fg
                (0xa8, 0x99, 0x84), // gray
                (0x83, 0xa5, 0x98), // blue
                (0xb8, 0xbb, 0x26), // green
                (0xfa, 0xbd, 0x2f), // yellow
                (0xfb, 0x49, 0x34), // red
                (0x28, 0x28, 0x28), // on-accent ≈ bg
            ),
        ),
        spec(
            "gruvbox-light",
            "Gruvbox Light",
            rgb_palette(
                (0xfb, 0xf1, 0xc7), // bg
                (0x3c, 0x38, 0x36), // fg
                (0x7c, 0x6f, 0x64), // gray
                (0x07, 0x66, 0x78), // blue
                (0x79, 0x74, 0x0e), // green
                (0xb5, 0x76, 0x14), // yellow
                (0x9d, 0x00, 0x06), // red
                (0xfb, 0xf1, 0xc7), // on-accent ≈ bg
            ),
        ),
        // --- Nord (https://www.nordtheme.com/) --------------------------
        spec(
            "nord",
            "Nord",
            rgb_palette(
                (0x2e, 0x34, 0x40), // nord0
                (0xd8, 0xde, 0xe9), // nord4
                (0x81, 0x8f, 0xa3), // dimmed snow
                (0x88, 0xc0, 0xd0), // nord8
                (0xa3, 0xbe, 0x8c), // nord14 green
                (0xeb, 0xcb, 0x8b), // nord13 yellow
                (0xbf, 0x61, 0x6a), // nord11 red
                (0x2e, 0x34, 0x40), // on-accent ≈ nord0
            ),
        ),
        // --- Dracula (https://draculatheme.com/contribute) --------------
        spec(
            "dracula",
            "Dracula",
            rgb_palette(
                (0x28, 0x2a, 0x36), // background
                (0xf8, 0xf8, 0xf2), // foreground
                (0x62, 0x72, 0xa4), // comment
                (0xbd, 0x93, 0xf9), // purple
                (0x50, 0xfa, 0x7b), // green
                (0xf1, 0xfa, 0x8c), // yellow
                (0xff, 0x55, 0x55), // red
                (0x28, 0x2a, 0x36), // on-accent ≈ background
            ),
        ),
        // --- Tokyo Night (https://github.com/folke/tokyonight.nvim) -----
        spec(
            "tokyo-night",
            "Tokyo Night",
            rgb_palette(
                (0x1a, 0x1b, 0x26), // bg
                (0xc0, 0xca, 0xf5), // fg
                (0x56, 0x5f, 0x89), // comment
                (0x7a, 0xa2, 0xf7), // blue
                (0x9e, 0xce, 0x6a), // green
                (0xe0, 0xaf, 0x68), // yellow
                (0xf7, 0x76, 0x8e), // red
                (0x1a, 0x1b, 0x26), // on-accent ≈ bg
            ),
        ),
        // --- Solarized (https://ethanschoonover.com/solarized/) ---------
        spec(
            "solarized-dark",
            "Solarized Dark",
            rgb_palette(
                (0x00, 0x2b, 0x36), // base03
                (0x83, 0x94, 0x96), // base0
                (0x58, 0x6e, 0x75), // base01
                (0x26, 0x8b, 0xd2), // blue
                (0x85, 0x99, 0x00), // green
                (0xb5, 0x89, 0x00), // yellow
                (0xdc, 0x32, 0x2f), // red
                (0x00, 0x2b, 0x36), // on-accent ≈ base03
            ),
        ),
        spec(
            "solarized-light",
            "Solarized Light",
            rgb_palette(
                (0xfd, 0xf6, 0xe3), // base3
                (0x65, 0x7b, 0x83), // base00
                (0x93, 0xa1, 0xa1), // base1
                (0x26, 0x8b, 0xd2), // blue
                (0x85, 0x99, 0x00), // green
                (0xb5, 0x89, 0x00), // yellow
                (0xdc, 0x32, 0x2f), // red
                (0xfd, 0xf6, 0xe3), // on-accent ≈ base3
            ),
        ),
    ]
}

// ---- WCAG self-check (built-in dark/light only) -----------------------

/// Backgrounds used for the contrast self-check.
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
    fn key_hints_cycle_round_trips() {
        let mut h = KeyHintsMode::Rich;
        for _ in 0..KeyHintsMode::ALL.len() {
            h = h.next();
        }
        assert_eq!(h, KeyHintsMode::Rich);
    }

    #[test]
    fn key_hints_label_roundtrip_is_lossless() {
        for h in KeyHintsMode::ALL {
            assert_eq!(KeyHintsMode::from_label(h.label()), Some(h));
        }
        assert_eq!(KeyHintsMode::from_label("RICH"), Some(KeyHintsMode::Rich));
        assert_eq!(KeyHintsMode::from_label("nope"), None);
    }

    #[test]
    fn catalog_resolves_known_ids() {
        let cat = ThemeCatalog::builtin();
        // A representative slice of the built-ins must be present.
        for id in [
            "auto",
            "dark",
            "light",
            "catppuccin-mocha",
            "gruvbox-dark",
            "nord",
            "dracula",
            "tokyo-night",
            "solarized-light",
        ] {
            assert!(cat.contains(id), "built-in {id} missing from catalog");
        }
        assert_eq!(cat.ids().first().map(String::as_str), Some("auto"));
    }

    #[test]
    fn unknown_id_falls_back_to_auto_palette() {
        let cat = ThemeCatalog::builtin();
        let pal = cat.palette("does-not-exist");
        assert_eq!(pal, auto_palette());
        // Auto leaves the canvas to the terminal.
        assert_eq!(pal.bg(), Color::Reset);
        assert_eq!(pal.color(Accent::Body), Color::Reset);
    }

    #[test]
    fn custom_theme_appends_and_overrides() {
        let custom = vec![
            ThemeSpec {
                id: "my-theme".into(),
                label: "My Theme".into(),
                palette: rgb_palette(
                    (1, 2, 3),
                    (4, 5, 6),
                    (7, 8, 9),
                    (10, 11, 12),
                    (13, 14, 15),
                    (16, 17, 18),
                    (19, 20, 21),
                    (22, 23, 24),
                ),
            },
            // Overrides the built-in `dark`.
            ThemeSpec {
                id: "dark".into(),
                label: "Dark (mine)".into(),
                palette: rgb_palette(
                    (0, 0, 0),
                    (255, 255, 255),
                    (1, 1, 1),
                    (2, 2, 2),
                    (3, 3, 3),
                    (4, 4, 4),
                    (5, 5, 5),
                    (6, 6, 6),
                ),
            },
        ];
        let cat = ThemeCatalog::with_custom(custom);
        assert!(cat.contains("my-theme"), "custom theme must be appended");
        assert_eq!(
            cat.label_for("dark"),
            "Dark (mine)",
            "custom overrides built-in label"
        );
        assert_eq!(cat.palette("dark").bg(), Color::Rgb(0, 0, 0));
        // Built-in count unchanged + 1 appended (override replaced in place).
        assert_eq!(cat.specs().len(), builtin_specs().len() + 1);
    }

    /// Acceptance: only the built-in `dark` / `light` palettes are
    /// asserted to clear WCAG-AA (≥4.5:1) for body-class text against
    /// their own canvas. Third-party palettes are trusted as-authored.
    #[test]
    fn wcag_aa_passes_for_builtin_dark_light() {
        let cat = ThemeCatalog::builtin();
        let cases: [(&str, (u8, u8, u8)); 2] = [("dark", DARK_BG), ("light", LIGHT_BG)];
        let slots = [
            Accent::Primary,
            Accent::Ok,
            Accent::Warn,
            Accent::Error,
            Accent::Body,
        ];
        for (id, bg) in cases {
            let pal = cat.palette(id);
            for slot in slots {
                let c = pal.color(slot);
                let rgb = rgb_of(c)
                    .unwrap_or_else(|| panic!("palette accent {slot:?} in {id} must be Rgb"));
                let ratio = contrast_ratio(rgb, bg);
                assert!(
                    ratio >= 4.5,
                    "{id}::{slot:?} contrast {ratio:.2}:1 < AA 4.5:1 (fg #{:02x}{:02x}{:02x} on bg #{:02x}{:02x}{:02x})",
                    rgb.0, rgb.1, rgb.2, bg.0, bg.1, bg.2,
                );
            }
        }
    }
}
