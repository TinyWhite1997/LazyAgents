//! Socket and runtime-dir discovery (architecture §11.2 / §11.3).
//!
//! Lives in `la-ipc` (not `la-daemon`) because BOTH the daemon and the
//! client need to agree on the path: the daemon binds, the client
//! connects. Putting it in the transport crate also keeps the la-tui
//! dependency rule (architecture §2.1: la-tui depends only on la-proto +
//! la-ipc) intact.
//!
//! Lookup order for the runtime dir (Unix):
//!
//! 1. `$LAZYAGENTS_RUNTIME_DIR` — explicit override for tests / sandboxed
//!    runs (the integration suite points two daemons at distinct
//!    tempdirs to prove they don't conflict, M1.7 acceptance).
//! 2. `$XDG_RUNTIME_DIR/lazyagents` — standard freedesktop runtime root
//!    (`/run/user/$UID/lazyagents`).
//! 3. `$TMPDIR/lazyagents-$UID` (or `/tmp/lazyagents-$UID`) — fallback.
//!
//! Architecture §11.3: the socket file name embeds the protocol major,
//! e.g. `lad-1.sock`. A future v2 daemon binds `lad-2.sock` and the two
//! coexist.

use std::path::{Path, PathBuf};

/// Environment variable to override the runtime (socket) directory.
pub const ENV_RUNTIME_DIR: &str = "LAZYAGENTS_RUNTIME_DIR";

/// Directory name appended to the chosen root for daemon-owned files.
pub const APP_DIR_NAME: &str = "lazyagents";

/// Resolved location returned by [`SocketDiscovery::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketLocation {
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
}

/// Compose the per-protocol-major socket path. Cheap to construct.
#[derive(Debug, Clone)]
pub struct SocketDiscovery {
    /// Protocol major embedded in the socket file name. Defaults to
    /// [`la_proto::PROTOCOL_VERSION`].
    pub protocol_major: String,
    /// Explicit override; bypasses the env-var lookup chain. Useful for
    /// `--socket /path/explicit.sock` CLI flags.
    pub override_path: Option<PathBuf>,
}

impl Default for SocketDiscovery {
    fn default() -> Self {
        Self {
            protocol_major: la_proto::PROTOCOL_VERSION.to_string(),
            override_path: None,
        }
    }
}

impl SocketDiscovery {
    /// Pin a specific protocol major. Used by the integration suite to
    /// bring up "v1" and "v2" daemons side by side without depending on
    /// la-proto's compile-time constant.
    pub fn with_protocol_major(major: impl Into<String>) -> Self {
        Self {
            protocol_major: major.into(),
            override_path: None,
        }
    }

    /// Pin an absolute socket path. Skips env-var discovery.
    ///
    /// The path is transport-agnostic: it is the endpoint identifier the
    /// daemon and clients agree on, NOT a UDS-only filesystem path. On
    /// Unix it becomes the UDS socket file; on Windows it is mapped to a
    /// Named Pipe via [`crate::transport::endpoint_for`]
    /// (`lad-1.sock` → `\\.\pipe\lazyagents-lad-1`). Callers that need
    /// the corresponding transport endpoint must route the resolved path
    /// through `endpoint_for` rather than constructing `Endpoint::uds`
    /// directly, so the two platforms stay in sync.
    pub fn with_override(path: impl Into<PathBuf>) -> Self {
        Self {
            protocol_major: la_proto::PROTOCOL_VERSION.to_string(),
            override_path: Some(path.into()),
        }
    }

    /// Resolve runtime dir + socket path. Pure function — does NOT touch
    /// the filesystem (use [`ensure_runtime_dir`] separately if you need
    /// the dir to exist before binding).
    pub fn resolve(&self) -> SocketLocation {
        if let Some(path) = &self.override_path {
            let runtime_dir = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            return SocketLocation {
                runtime_dir,
                socket_path: path.clone(),
            };
        }
        let runtime_dir = default_runtime_dir();
        let socket_path = runtime_dir.join(socket_file_name(&self.protocol_major));
        SocketLocation {
            runtime_dir,
            socket_path,
        }
    }
}

/// Socket file name for the given protocol major. Single source of truth
/// for the `lad-<N>.sock` format.
pub fn socket_file_name(major: &str) -> String {
    format!("lad-{major}.sock")
}

/// Default socket path using the protocol major baked into la-proto.
pub fn default_socket_path() -> PathBuf {
    SocketDiscovery::default().resolve().socket_path
}

/// Runtime directory using the env-var lookup chain.
pub fn default_runtime_dir() -> PathBuf {
    if let Some(dir) = env_path(ENV_RUNTIME_DIR) {
        return dir;
    }
    if let Some(xdg) = env_path("XDG_RUNTIME_DIR") {
        return xdg.join(APP_DIR_NAME);
    }
    tmp_fallback_dir()
}

/// Ensure the runtime directory exists with restrictive permissions
/// (Unix: 0o700). Idempotent. Best-effort chmod — failures on
/// filesystems that don't honour mode bits are ignored.
pub fn ensure_runtime_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::metadata(dir)?;
        let mut perm = meta.permissions();
        if perm.mode() & 0o777 != 0o700 {
            perm.set_mode(0o700);
            let _ = std::fs::set_permissions(dir, perm);
        }
    }
    Ok(())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn tmp_fallback_dir() -> PathBuf {
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    #[cfg(unix)]
    {
        // Use libc's getuid directly so we don't pull in nix's `unistd`
        // feature just for the user id.
        // SAFETY: getuid is a signal-safe syscall with no preconditions.
        let uid = unsafe { libc_getuid() };
        tmp.join(format!("{APP_DIR_NAME}-{uid}"))
    }
    #[cfg(not(unix))]
    {
        tmp.join(APP_DIR_NAME)
    }
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_file_name_embeds_major() {
        assert_eq!(socket_file_name("1"), "lad-1.sock");
        assert_eq!(socket_file_name("42"), "lad-42.sock");
    }

    #[test]
    fn distinct_majors_get_distinct_paths() {
        let v1 = SocketDiscovery::with_protocol_major("1").resolve();
        let v2 = SocketDiscovery::with_protocol_major("2").resolve();
        assert_eq!(v1.runtime_dir, v2.runtime_dir);
        assert_ne!(v1.socket_path, v2.socket_path);
    }

    #[test]
    fn override_short_circuits_env_lookup() {
        let d = SocketDiscovery::with_override("/tmp/explicit.sock");
        let loc = d.resolve();
        assert_eq!(
            loc.socket_path,
            std::path::PathBuf::from("/tmp/explicit.sock")
        );
        assert_eq!(loc.runtime_dir, std::path::PathBuf::from("/tmp"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_runtime_dir_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("lazyagents");
        ensure_runtime_dir(&runtime).unwrap();

        let mode = std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
