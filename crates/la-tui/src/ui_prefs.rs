//! WEK-42 / M4.3 — read/write the `[ui]` section of
//! `$XDG_CONFIG_HOME/lazyagents/config.toml`.
//!
//! Architecture §11.1 sketches:
//!
//! ```toml
//! [ui]
//! theme = "auto"          # auto | dark | light
//! key_hints = "rich"      # rich | compact | hidden
//! compact = false         # M4.3 addition
//! ```
//!
//! We intentionally parse only the `[ui]` table and merge it back into
//! the existing TOML document on save — every other `[daemon]` /
//! `[scheduler]` / `[adapters.*]` section the daemon owns must survive a
//! TUI write untouched. The `toml` crate's `preserve_order` feature plus
//! `toml::value::Table` round-tripping is what guarantees that. Parsing
//! is best-effort: an unreadable file or malformed `[ui]` table yields
//! [`UiPrefs::default()`] so the TUI never refuses to start because of a
//! config typo.
//!
//! Architecture §2.1 forbids la-tui from depending on la-storage or
//! la-core, so this module owns the (small) TOML wrangling itself instead
//! of routing through a daemon RPC. The trade-off is conscious: the
//! daemon also reads `config.toml`, but `[ui]` is purely a client-side
//! concern (no daemon code reads `[ui]`) and the file is per-user, not
//! shared mutable state — a stale read between processes is harmless.

use std::path::{Path, PathBuf};

use crate::theme::{KeyHintsMode, Theme};

/// In-memory shape of the persisted `[ui]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiPrefs {
    pub theme: Theme,
    pub key_hints: KeyHintsMode,
    pub compact: bool,
}

impl Default for UiPrefs {
    fn default() -> Self {
        // Defaults mirror architecture §11.1's example.
        Self {
            theme: Theme::Auto,
            key_hints: KeyHintsMode::Rich,
            compact: false,
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
/// `theme = "moonbeam"` typo demotes only that one field.
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
        if let Some(t) = Theme::from_label(s) {
            out.theme = t;
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
    out
}

/// Persist `prefs` to `path`, preserving every other section that
/// already lives in the file. Creates parent directories on demand.
/// Returns the error verbatim so the caller can decide to log / toast;
/// the App treats a save failure as "in-memory pref still applies, user
/// will see it next launch only if they fix the underlying issue".
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

    let root = doc
        .as_table_mut()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "root is not a table"))?;

    // Overwrite `[ui]` in place. We rebuild the table from scratch
    // (instead of patching individual keys) so removed-from-schema keys
    // do not linger in old configs forever.
    let mut ui = toml::value::Table::new();
    ui.insert("theme".into(), toml::Value::String(prefs.theme.label().into()));
    ui.insert(
        "key_hints".into(),
        toml::Value::String(prefs.key_hints.label().into()),
    );
    ui.insert("compact".into(), toml::Value::Boolean(prefs.compact));
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
theme = "dark"
key_hints = "compact"
compact = true
extra_unknown = "ignored"
"#,
        );
        let prefs = load(&path);
        assert_eq!(prefs.theme, Theme::Dark);
        assert_eq!(prefs.key_hints, KeyHintsMode::Compact);
        assert!(prefs.compact);
    }

    #[test]
    fn malformed_key_falls_back_per_field() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            &dir,
            "c.toml",
            r#"
[ui]
theme = "moonbeam"
key_hints = "rich"
"#,
        );
        let prefs = load(&path);
        assert_eq!(prefs.theme, Theme::Auto, "unknown theme falls back");
        assert_eq!(prefs.key_hints, KeyHintsMode::Rich);
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
            theme: Theme::Light,
            key_hints: KeyHintsMode::Hidden,
            compact: true,
        };
        save(&path, &prefs).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        // Daemon section survives verbatim.
        assert!(raw.contains("log_level"), "lost [daemon].log_level: {raw}");
        assert!(
            raw.contains("global_max_concurrent_runs"),
            "lost [scheduler] section: {raw}"
        );
        // And the new [ui] table parses back identically.
        let reloaded = load(&path);
        assert_eq!(reloaded, prefs);
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("nested").join("deep").join("c.toml");
        save(
            &nested,
            &UiPrefs {
                theme: Theme::Dark,
                key_hints: KeyHintsMode::Rich,
                compact: false,
            },
        )
        .unwrap();
        let reloaded = load(&nested);
        assert_eq!(reloaded.theme, Theme::Dark);
    }

    #[test]
    fn save_refuses_to_overwrite_malformed_toml() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "c.toml", "this = is = not = toml");
        let err = save(&path, &UiPrefs::default()).expect_err("malformed input must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // Original content must still be on disk.
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
                theme: Theme::Light,
                key_hints: KeyHintsMode::Compact,
                compact: false,
            },
        )
        .unwrap();
        let reloaded = load(&path);
        assert_eq!(reloaded.theme, Theme::Light);
        assert_eq!(reloaded.key_hints, KeyHintsMode::Compact);
    }
}
