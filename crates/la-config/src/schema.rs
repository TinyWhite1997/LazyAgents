//! TOML schema for `config.toml`. Every struct uses
//! `#[serde(deny_unknown_fields)]` so a typo (`sokcet_path`) or a
//! removed-field carry-over fails `lad config check` instead of being
//! silently ignored — addendum S3 hard requirement.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Errors surfaced by `Config::parse_str` / `Config::load_file`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Wraps `toml::de::Error` so callers can render the row/col + message
    /// (used by `lad config check`'s "unknown field `<name>`, expected one
    /// of …" output).
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    /// I/O failure reading the file (path printed by the caller).
    #[error("config io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Root `config.toml` shape. All sections optional — a file with only
/// `[ui]` is valid; an empty file is valid; a missing file is treated by
/// callers as "use all defaults".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// `[daemon]` table — socket / state dir / logging.
    pub daemon: DaemonConfig,
    /// `[scheduler]` table — global concurrency + archive.
    pub scheduler: SchedulerConfig,
    /// `[worktree]` table — git worktree retention.
    pub worktree: WorktreeConfig,
    /// `[adapters.*]` tables — one per known backend.
    pub adapters: AdaptersConfig,
    /// `[ui]` table — TUI client preferences (la-tui owns the consumer
    /// today; mirrored here so `lad config check` validates the whole
    /// file, not just the daemon-owned sections).
    pub ui: UiConfig,
}

impl Config {
    /// Parse a TOML string into a [`Config`]. Errors carry serde-toml's
    /// row/col so `lad config check` can pinpoint the typo.
    pub fn parse_str(raw: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(raw)?)
    }

    /// Read + parse a file. A missing file is **not** an error here —
    /// callers (`Resolved::load`) treat absent as "use defaults".
    pub fn load_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        Self::parse_str(&raw)
    }
}

/// `[daemon]` section. Defaults match M1.7 behaviour so an empty file
/// behaves identically to the pre-M4.2 daemon.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// Absolute path to the listening UDS / Named Pipe. `None` = OS
    /// default position (`SocketDiscovery::default()` resolves it).
    pub socket_path: Option<PathBuf>,
    /// SQLite + spilled-transcripts directory. `None` = the la-daemon
    /// default (`$XDG_STATE_HOME/lazyagents`).
    pub state_dir: Option<PathBuf>,
    /// `trace|debug|info|warn|error`.
    pub log_level: LogLevel,
    /// Output format for tracing.
    pub log_format: LogFormat,
    /// Architecture §10.1: TCP listener is a v1.x feature. Non-empty
    /// here is rejected at daemon startup with an instructive error so
    /// "I configured a TCP port and nothing happened" cannot occur.
    pub listen_tcp: String,
}

/// Logging verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Trace
    Trace,
    /// Debug
    Debug,
    /// Info (default)
    #[default]
    Info,
    /// Warn
    Warn,
    /// Error
    Error,
}

impl LogLevel {
    /// Lowercase wire label matching `EnvFilter` syntax.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    /// Parse a wire label. Returns `None` for unknown strings so callers
    /// can render their own error message with the offending value.
    pub fn parse_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(LogLevel::Trace),
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" | "warning" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

/// Output format for tracing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Production default — single-line JSON, joined with daemon
    /// `trace_id` correlation.
    #[default]
    Json,
    /// Developer-only — compact human-readable format.
    Compact,
}

impl LogFormat {
    /// Wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            LogFormat::Json => "json",
            LogFormat::Compact => "compact",
        }
    }

    /// Parse a wire label.
    pub fn parse_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" => Some(LogFormat::Json),
            "compact" => Some(LogFormat::Compact),
            _ => None,
        }
    }
}

/// `[scheduler]` section — surfaces M3 admission gate knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SchedulerConfig {
    /// Single admission gate cap; matches M3 [WEK-57] default.
    pub global_max_concurrent_runs: u32,
    /// CPU load gate — soft throttle below this load.
    pub cpu_load_throttle: f32,
    /// Archive directory (`None` = `$state_dir/archive`).
    pub archive_dir: Option<PathBuf>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            global_max_concurrent_runs: 8,
            cpu_load_throttle: 4.0,
            archive_dir: None,
        }
    }
}

/// `[worktree]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorktreeConfig {
    /// Prune git worktrees older than this many days.
    pub prune_after_days: u32,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            prune_after_days: 7,
        }
    }
}

/// `[adapters.*]` umbrella. Known backends (`claude / codex / opencode`)
/// are typed so `deny_unknown_fields` catches typos *inside* their
/// tables. Brand-new backends a future daemon may add land in `extra`
/// instead of failing the schema check, so a user can stage
/// `[adapters.gemini]` config ahead of the daemon upgrade that
/// recognises the backend; the daemon emits a runtime warning when it
/// sees an unknown adapter id, which is the right place for that
/// diagnostic. Section labels (`[adapters.<backend>]`) themselves are
/// permissive on purpose — only the inner table fields are strict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdaptersConfig {
    /// Anthropic Claude CLI.
    pub claude: ClaudeConfig,
    /// OpenAI Codex CLI.
    pub codex: CodexConfig,
    /// Opencode CLI.
    pub opencode: OpencodeConfig,
    /// Adapter tables whose id is not yet typed by la-config. Values
    /// share the generic [`AdapterConfig`] shape; per-backend keys that
    /// don't fit `command / extra_args / env` will fail
    /// `deny_unknown_fields` on `AdapterConfig` (so typo-resistance is
    /// preserved at the field level even when the backend id is new).
    #[serde(flatten)]
    pub extra: BTreeMap<String, AdapterConfig>,
}

/// Generic per-adapter knobs shared across the typed backends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AdapterConfig {
    /// Executable name or absolute path. `None` = adapter default.
    pub command: Option<String>,
    /// Extra args prepended to every spawn.
    pub extra_args: Vec<String>,
    /// Extra env vars injected on every spawn.
    pub env: BTreeMap<String, String>,
}

/// Claude-specific adapter config (no extra fields today; carved out so
/// `[adapters.claude]` matches the architecture-§11.1 schema exactly).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClaudeConfig {
    /// Shared adapter knobs.
    #[serde(flatten)]
    pub base: AdapterConfig,
}

/// Codex-specific adapter config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CodexConfig {
    /// Shared adapter knobs.
    #[serde(flatten)]
    pub base: AdapterConfig,
    /// Pass `--json` to codex when possible.
    pub prefer_json_mode: bool,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            base: AdapterConfig::default(),
            prefer_json_mode: true,
        }
    }
}

/// Opencode-specific adapter config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OpencodeConfig {
    /// Shared adapter knobs.
    #[serde(flatten)]
    pub base: AdapterConfig,
}

/// `[ui]` section — mirrors the la-tui in-memory shape so the daemon's
/// schema check covers the whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiConfig {
    /// `auto | dark | light`.
    pub theme: String,
    /// `rich | compact | hidden`.
    pub key_hints: String,
    /// Compact transcript mode.
    pub compact: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "auto".to_string(),
            key_hints: "rich".to_string(),
            compact: false,
        }
    }
}
