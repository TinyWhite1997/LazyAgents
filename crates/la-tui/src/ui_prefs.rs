//! WEK-42 / M4.3 — read/write the `[ui]` section of
//! `$XDG_CONFIG_HOME/lazyagents/config.toml`.
//!
//! Architecture §11.1 sketches:
//!
//! ```toml
//! [ui]
//! theme = "auto"          # any built-in or custom theme id
//! key_hints = "rich"      # rich | compact | hidden
//! compact = false
//!
//! # Optional user-defined palettes, shown in the theme picker:
//! [[ui.custom_theme]]
//! id = "my-theme"
//! label = "My Theme"
//! bg = "#1e1e2e"
//! fg = "#cdd6f4"
//! muted = "#a6adc8"
//! primary = "#89b4fa"
//! ok = "#a6e3a1"
//! warn = "#f9e2af"
//! error = "#f38ba8"
//! on_accent = "#1e1e2e"
//! ```
//!
//! We intentionally parse only the `[ui]` table and merge it back into
//! the existing TOML document on save — every other `[daemon]` /
//! `[scheduler]` / `[adapters.*]` section the daemon owns must survive a
//! TUI write untouched. Parsing is best-effort: an unreadable file or
//! malformed `[ui]` table yields [`UiPrefs::default()`] so the TUI never
//! refuses to start because of a config typo, and a single malformed
//! custom-theme entry is skipped rather than poisoning the whole table.
//!
//! Architecture §2.1 forbids la-tui from depending on la-storage or
//! la-core, so this module owns the (small) TOML wrangling itself instead
//! of routing through a daemon RPC. `[ui]` (incl. `[[ui.custom_theme]]`)
//! is purely a client-side concern — no daemon code reads it — and the
//! file is per-user, so a stale read between processes is harmless.

use std::path::{Path, PathBuf};

use crate::theme::{KeyHintsMode, Palette, ThemeSpec, DEFAULT_THEME_ID};

/// In-memory shape of the persisted `[ui]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiPrefs {
    /// Active theme **id** — a built-in (`"dark"`, `"catppuccin-mocha"`,
    /// …) or a custom theme defined in `custom`. Free-form: an unknown id
    /// demotes to the `auto` palette at render time.
    pub theme: String,
    pub key_hints: KeyHintsMode,
    pub compact: bool,
    /// User-defined palettes parsed from `[[ui.custom_theme]]`. Empty by
    /// default.
    pub custom: Vec<ThemeSpec>,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME_ID.to_string(),
            key_hints: KeyHintsMode::Rich,
            compact: false,
            custom: Vec::new(),
        }
    }
}

/// Pick the on-disk config path. Honours `LAZYAGENTS_CONFIG_HOME` first
/// (test override + script-friendly), then `$XDG_CONFIG_HOME`, then the
/// `$HOME/.config` XDG fallback. Returns `None` only when none of those
/// can be resolved (extremely degraded environment) — callers treat that
/// as "no persistence available, in-memory only".
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LAZYAGENTS_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("config.toml"));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("lazyagents").join("config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("lazyagents")
                .join("config.toml"),
        );
    }
    None
}

/// Load prefs from `path`. Returns [`UiPrefs::default()`] if the file
/// doesn't exist, is unreadable, or has an unparseable `[ui]` table.
/// Individual key parse failures fall back to the per-field default so a
/// `key_hints = "loud"` typo demotes only that one field; the active
/// `theme` is stored verbatim (validated at render time against the
/// catalog).
pub fn load(path: &Path) -> UiPrefs {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return UiPrefs::default();
    };
    let Ok(doc) = raw.parse::<toml::Value>() else {
        return UiPrefs::default();
    };
    let Some(ui) = doc.get("ui").and_then(|v| v.as_table()) else {
        return UiPrefs::default();
    };
    let mut out = UiPrefs::default();
    if let Some(s) = ui.get("theme").and_then(|v| v.as_str()) {
        // Store the id verbatim — even an unknown one. The catalog
        // resolves unknown ids to the `auto` palette at render time, and
        // keeping the raw string means a custom theme defined later in
        // the same file still matches.
        if !s.trim().is_empty() {
            out.theme = s.trim().to_string();
        }
    }
    if let Some(s) = ui.get("key_hints").and_then(|v| v.as_str()) {
        if let Some(h) = KeyHintsMode::from_label(s) {
            out.key_hints = h;
        }
    }
    if let Some(b) = ui.get("compact").and_then(|v| v.as_bool()) {
        out.compact = b;
    }
    if let Some(arr) = ui.get("custom_theme").and_then(|v| v.as_array()) {
        for entry in arr {
            if let Some(spec) = parse_custom_theme(entry) {
                out.custom.push(spec);
            }
        }
    }
    out
}

/// Parse a hex colour string (`"#rrggbb"` or `"rrggbb"`) into an sRGB
/// triple. Returns `None` for any malformed input.
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Materialise one `[[ui.custom_theme]]` table into a [`ThemeSpec`].
/// Returns `None` (skipping the entry) when `id` is missing/empty or any
/// required colour key is missing or malformed.
fn parse_custom_theme(entry: &toml::Value) -> Option<ThemeSpec> {
    let t = entry.as_table()?;
    let id = t.get("id").and_then(|v| v.as_str())?.trim();
    if id.is_empty() {
        return None;
    }
    let label = t
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string());
    let hex = |key: &str| -> Option<(u8, u8, u8)> {
        t.get(key).and_then(|v| v.as_str()).and_then(parse_hex)
    };
    let bg = hex("bg")?;
    let fg = hex("fg")?;
    let muted = hex("muted")?;
    let primary = hex("primary")?;
    let ok = hex("ok")?;
    let warn = hex("warn")?;
    let error = hex("error")?;
    // `on_accent` is optional — default to the background colour, which
    // matches how the built-ins set their on-chip text.
    let on_accent = hex("on_accent").unwrap_or(bg);
    Some(ThemeSpec {
        id: id.to_string(),
        label,
        palette: Palette::from_rgb(bg, fg, muted, primary, ok, warn, error, on_accent),
    })
}

/// Persist `prefs` to `path`, preserving every other section that
/// already lives in the file (including any `[[ui.custom_theme]]` array,
/// which the TUI never edits — it only reads). Creates parent
/// directories on demand. Returns the error verbatim so the caller can
/// decide to log / toast; the App treats a save failure as "in-memory
/// pref still applies, user will see it next launch only if they fix the
/// underlying issue".
pub fn save(path: &Path, prefs: &UiPrefs) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Read-modify-write so the daemon's `[daemon]` / `[scheduler]` /
    // `[adapters.*]` sections survive untouched. A missing or unparseable
    // file is treated as "start from an empty document"; we will NOT
    // attempt to repair malformed TOML — that's the daemon's job, and
    // overwriting it from the TUI would silently nuke the user's config.
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml::Value = match existing.parse() {
        Ok(v) => v,
        Err(_) if existing.trim().is_empty() => empty_table(),
        Err(e) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refusing to overwrite malformed config.toml: {e}"),
            ));
        }
    };

    let root = doc.as_table_mut().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "root is not a table")
    })?;

    // Preserve a user-authored `custom_theme` array verbatim: the TUI's
    // theme picker selects themes but never edits palette definitions, so
    // rebuilding the `[ui]` table from scratch must not drop them.
    let preserved_custom = root
        .get("ui")
        .and_then(|v| v.as_table())
        .and_then(|ui| ui.get("custom_theme"))
        .cloned();

    // Overwrite `[ui]` in place. We rebuild the scalar keys from scratch
    // (instead of patching individually) so removed-from-schema keys do
    // not linger in old configs forever, then re-attach the preserved
    // custom-theme array.
    let mut ui = toml::value::Table::new();
    ui.insert("theme".into(), toml::Value::String(prefs.theme.clone()));
    ui.insert(
        "key_hints".into(),
        toml::Value::String(prefs.key_hints.label().into()),
    );
    ui.insert("compact".into(), toml::Value::Boolean(prefs.compact));
    if let Some(custom) = preserved_custom {
        ui.insert("custom_theme".into(), custom);
    }
    root.insert("ui".into(), toml::Value::Table(ui));

    let serialized = toml::to_string_pretty(&doc).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("encode toml: {e}"))
    })?;

    // Atomic-ish write: write to a sibling tempfile then rename. Stops a
    // crash mid-write from leaving a half-truncated config that the
    // daemon would then reject.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, serialized.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::value::Table::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn default_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let prefs = load(&dir.path().join("nope.toml"));
        assert_eq!(prefs, UiPrefs::default());
    }

    #[test]
    fn parses_ui_section_and_ignores_unknown_keys() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "c.toml",
            r#"
[daemon]
log_level = "info"

[ui]
theme = "catppuccin-mocha"
key_hints = "compact"
compact = true
extra_unknown = "ignored"
"#,
        );
        let prefs = load(&path);
        assert_eq!(prefs.theme, "catppuccin-mocha");
        assert_eq!(prefs.key_hints, KeyHintsMode::Compact);
        assert!(prefs.compact);
    }

    #[test]
    fn malformed_key_hints_falls_back_but_theme_is_verbatim() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "c.toml",
            r#"
[ui]
theme = "some-future-theme"
key_hints = "loud"
"#,
        );
        let prefs = load(&path);
        // Unknown theme ids are kept verbatim (resolved to auto at render).
        assert_eq!(prefs.theme, "some-future-theme");
        // Unknown key_hints demotes to the default.
        assert_eq!(prefs.key_hints, KeyHintsMode::Rich);
    }

    #[test]
    fn parse_hex_accepts_both_forms_and_rejects_garbage() {
        assert_eq!(parse_hex("#1e1e2e"), Some((0x1e, 0x1e, 0x2e)));
        assert_eq!(parse_hex("CDD6F4"), Some((0xcd, 0xd6, 0xf4)));
        assert_eq!(parse_hex("  #ffffff  "), Some((0xff, 0xff, 0xff)));
        assert_eq!(parse_hex("#fff"), None, "3-digit shorthand unsupported");
        assert_eq!(parse_hex("#gggggg"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn parses_custom_theme_array() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "c.toml",
            r##"
[ui]
theme = "my-theme"
key_hints = "rich"
compact = false

[[ui.custom_theme]]
id = "my-theme"
label = "My Theme"
bg = "#101010"
fg = "#fafafa"
muted = "#808080"
primary = "#3366ff"
ok = "#33cc66"
warn = "#ffcc33"
error = "#ff3344"

[[ui.custom_theme]]
id = "bad-theme"
bg = "not-a-color"
"##,
        );
        let prefs = load(&path);
        assert_eq!(prefs.theme, "my-theme");
        // The malformed second entry is skipped; only the valid one lands.
        assert_eq!(
            prefs.custom.len(),
            1,
            "malformed custom theme must be skipped"
        );
        let spec = &prefs.custom[0];
        assert_eq!(spec.id, "my-theme");
        assert_eq!(spec.label, "My Theme");
    }

    /// Acceptance: writing `[ui]` must not clobber a sibling `[daemon]`
    /// table. That's the entire reason we go read-modify-write instead of
    /// serializing only the prefs struct.
    #[test]
    fn save_preserves_other_sections() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "c.toml",
            r#"[daemon]
log_level = "info"
listen_tcp = ""

[scheduler]
global_max_concurrent_runs = 8
"#,
        );
        let prefs = UiPrefs {
            theme: "gruvbox-dark".into(),
            key_hints: KeyHintsMode::Hidden,
            compact: true,
            custom: Vec::new(),
        };
        save(&path, &prefs).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("log_level"), "lost [daemon].log_level: {raw}");
        assert!(
            raw.contains("global_max_concurrent_runs"),
            "lost [scheduler] section: {raw}"
        );
        let reloaded = load(&path);
        assert_eq!(reloaded.theme, "gruvbox-dark");
        assert_eq!(reloaded.key_hints, KeyHintsMode::Hidden);
        assert!(reloaded.compact);
    }

    /// Acceptance: the theme picker persists the selected id but must NOT
    /// drop a user's hand-authored `[[ui.custom_theme]]` definitions.
    #[test]
    fn save_preserves_custom_theme_array() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "c.toml",
            r##"[ui]
theme = "auto"
key_hints = "rich"
compact = false

[[ui.custom_theme]]
id = "my-theme"
label = "My Theme"
bg = "#101010"
fg = "#fafafa"
muted = "#808080"
primary = "#3366ff"
ok = "#33cc66"
warn = "#ffcc33"
error = "#ff3344"
"##,
        );
        // Simulate the picker selecting the custom theme.
        let prefs = UiPrefs {
            theme: "my-theme".into(),
            key_hints: KeyHintsMode::Rich,
            compact: false,
            custom: Vec::new(),
        };
        save(&path, &prefs).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("[[ui.custom_theme]]"),
            "custom_theme array must survive a picker save: {raw}"
        );
        let reloaded = load(&path);
        assert_eq!(reloaded.theme, "my-theme");
        assert_eq!(reloaded.custom.len(), 1, "custom theme lost on save");
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("nested").join("deep").join("c.toml");
        save(
            &nested,
            &UiPrefs {
                theme: "dark".into(),
                key_hints: KeyHintsMode::Rich,
                compact: false,
                custom: Vec::new(),
            },
        )
        .unwrap();
        let reloaded = load(&nested);
        assert_eq!(reloaded.theme, "dark");
    }

    #[test]
    fn save_refuses_to_overwrite_malformed_toml() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "c.toml", "this = is = not = toml");
        let err = save(&path, &UiPrefs::default()).expect_err("malformed input must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("not = toml"));
    }

    #[test]
    fn empty_file_is_treated_as_fresh_document() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "c.toml", "");
        save(
            &path,
            &UiPrefs {
                theme: "nord".into(),
                key_hints: KeyHintsMode::Compact,
                compact: false,
                custom: Vec::new(),
            },
        )
        .unwrap();
        let reloaded = load(&path);
        assert_eq!(reloaded.theme, "nord");
        assert_eq!(reloaded.key_hints, KeyHintsMode::Compact);
    }
}
