//! Re-exports of la-ipc's path discovery for ergonomic access from the
//! daemon and its tests. The canonical implementation lives in `la-ipc`
//! so the client (`la`) can use the same lookup without depending on
//! `la-daemon`.

pub use la_ipc::paths::{
    default_runtime_dir, default_socket_path, ensure_runtime_dir, socket_file_name,
    SocketDiscovery, SocketLocation, APP_DIR_NAME, ENV_RUNTIME_DIR,
};

use std::path::PathBuf;

/// Default data directory (SQLite + spilled transcripts).
///
/// Lookup order:
/// 1. `$LAZYAGENTS_DATA_DIR`
/// 2. `$XDG_DATA_HOME/lazyagents`
/// 3. `$HOME/.local/share/lazyagents`
/// 4. tmp fallback shared with the runtime dir.
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = env_path("LAZYAGENTS_DATA_DIR") {
        return dir;
    }
    if let Some(xdg) = env_path("XDG_DATA_HOME") {
        return xdg.join(APP_DIR_NAME);
    }
    if let Some(home) = env_path("HOME") {
        return home.join(".local/share").join(APP_DIR_NAME);
    }
    default_runtime_dir()
}

/// Convenience: per-version socket path using the default discovery rules.
pub fn socket_path_for_version(major: &str) -> PathBuf {
    SocketDiscovery::with_protocol_major(major)
        .resolve()
        .socket_path
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
