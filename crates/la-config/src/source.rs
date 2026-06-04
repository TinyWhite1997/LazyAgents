//! Track where each resolved value came from so `lad config show` can
//! print a comment per line. The precedence rule (DoD #2) is:
//!
//! ```text
//! CLI flag  >  env  >  config file  >  built-in default
//! ```
//!
//! Crucially, the M1.7 CLI flags (`--socket / --state-dir / --log-level`)
//! must continue to win — `Source::Cli` is the highest precedence and
//! the wrapper [`resolve`] short-circuits as soon as it sees one.

use std::fmt;

/// Provenance of a single resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// CLI flag passed on the command line.
    Cli,
    /// Read from an environment variable (e.g. `LAZYAGENTS_LOG_LEVEL`).
    Env,
    /// Loaded from `config.toml`.
    ConfigFile,
    /// Built-in default — neither CLI, env, nor file set it.
    Default,
}

impl Source {
    /// Comment string used by `lad config show` (`# from CLI flag`,
    /// `# from env`, `# from config`, `# from default`).
    pub fn label(self) -> &'static str {
        match self {
            Source::Cli => "from CLI flag",
            Source::Env => "from env",
            Source::ConfigFile => "from config",
            Source::Default => "from default",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// First non-None source wins. Order matches the precedence table —
/// callers pass `(cli, env, file)` and get back `(value, Source)` with
/// the layered default as the final fallback.
pub fn resolve<T: Clone>(
    cli: Option<T>,
    env: Option<T>,
    file: Option<T>,
    default: T,
) -> (T, Source) {
    if let Some(v) = cli {
        (v, Source::Cli)
    } else if let Some(v) = env {
        (v, Source::Env)
    } else if let Some(v) = file {
        (v, Source::ConfigFile)
    } else {
        (default, Source::Default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_beats_env_beats_file_beats_default() {
        assert_eq!(
            resolve(Some("cli"), Some("env"), Some("file"), "default"),
            ("cli", Source::Cli)
        );
        assert_eq!(
            resolve(None, Some("env"), Some("file"), "default"),
            ("env", Source::Env)
        );
        assert_eq!(
            resolve(None, None, Some("file"), "default"),
            ("file", Source::ConfigFile)
        );
        assert_eq!(
            resolve::<&str>(None, None, None, "default"),
            ("default", Source::Default)
        );
    }
}
