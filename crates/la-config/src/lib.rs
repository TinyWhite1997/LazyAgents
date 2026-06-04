//! `la-config` — TOML schema + load precedence for LazyAgents
//! (WEK-71 / M4.2; A4 + S3 of M4 brief v1.1 addendum).
//!
//! Owns three concerns:
//!
//! 1. The `[daemon] / [scheduler] / [worktree] / [adapters.*] / [ui]`
//!    schema as serde structs with `deny_unknown_fields`. Typo-resistant
//!    by construction.
//! 2. The four-tier load precedence `CLI flag > env > config file > built-in
//!    default` — exposed as a `Resolved<T>` value carrying the source so
//!    `lad config show` can annotate each line.
//! 3. The cross-platform `resolve_config_path()` chain (A4):
//!    `$LAZYAGENTS_CONFIG → $XDG_CONFIG_HOME/lazyagents/config.toml →
//!    $HOME/.config/lazyagents/config.toml →
//!    $HOME/Library/Application Support/lazyagents/config.toml` (macOS only) →
//!    `%APPDATA%\lazyagents\config.toml` (Windows).
//!
//! Lives in its own crate (instead of inside la-daemon) so future readers
//! — `la` for the `[ui]` table today; the M4.1 `lad install --service
//! launchd` plist writer tomorrow — can reuse `resolve_config_path()`
//! without dragging in the daemon-side runtime.

#![deny(missing_docs)]

pub mod paths;
pub mod schema;
pub mod source;

pub use paths::{config_path_for_install, resolve_config_path, ResolvedConfigPath};
pub use schema::{
    AdapterConfig, AdaptersConfig, ClaudeConfig, CodexConfig, Config, ConfigError, DaemonConfig,
    LogFormat, LogLevel, OpencodeConfig, SchedulerConfig, UiConfig, WorktreeConfig,
};
pub use source::Source;

/// Env var that overrides the config file path entirely (S3 addendum).
pub const ENV_CONFIG: &str = "LAZYAGENTS_CONFIG";

/// Env var that overrides `[daemon].socket_path` (S3 addendum).
pub const ENV_SOCKET: &str = "LAZYAGENTS_SOCKET";

/// Env var that overrides `[daemon].state_dir` (S3 addendum).
pub const ENV_STATE_DIR: &str = "LAZYAGENTS_STATE_DIR";

/// Env var that overrides `[daemon].log_level` (S3 addendum).
pub const ENV_LOG_LEVEL: &str = "LAZYAGENTS_LOG_LEVEL";

/// Env var that overrides `[daemon].log_format` (S3 addendum).
pub const ENV_LOG_FORMAT: &str = "LAZYAGENTS_LOG_FORMAT";

/// Deprecated alias for [`ENV_LOG_LEVEL`]; recognised in M4.2 with a
/// one-shot warning, removed in v1.1.
pub const ENV_LOG_LEVEL_DEPRECATED: &str = "LAZYAGENTS_LOG";
