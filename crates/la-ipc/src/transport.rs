//! Cross-platform IPC transport.
//!
//! On Unix we use `tokio::net::UnixListener` / `UnixStream` (the daemon binds
//! a socket file under `$XDG_RUNTIME_DIR/lazyagents/lad.sock` in production;
//! tests create one under `tempfile`). On Windows we use Named Pipes via
//! `tokio::net::windows::named_pipe`. The two platforms share the
//! [`StreamPair`] alias so callers can be transport-agnostic above this layer.
//!
//! Security note: Unix listeners bind under a temporary `umask(0o077)`, then
//! chmod the socket file to 0600 and verify accepted peers with `SO_PEERCRED`.
//! Windows listeners reject remote clients; the current v1 validation target
//! is Linux, so Windows SID ACL hardening remains behind the platform gate.
//!
//! The `Endpoint` enum exists so the same code path can describe both a UDS
//! path and a Named Pipe name without conditional compilation in callers.

use std::path::{Path, PathBuf};

use crate::IpcError;

/// Where to listen / connect.
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// Unix Domain Socket path (Linux, macOS).
    Uds(PathBuf),
    /// Windows Named Pipe (e.g. `\\.\pipe\lazyagents-lad`).
    NamedPipe(String),
}

impl Endpoint {
    /// Convenience: build a UDS endpoint from any path.
    pub fn uds(p: impl AsRef<Path>) -> Self {
        Endpoint::Uds(p.as_ref().to_path_buf())
    }
    /// Convenience: build a Named Pipe endpoint from any name.
    pub fn named_pipe(name: impl Into<String>) -> Self {
        Endpoint::NamedPipe(name.into())
    }
}

// ---------------- Unix implementation ----------------

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt as _;
    use tokio::net::{UnixListener, UnixStream};

    /// Listener handle.
    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
        /// Inode of the socket file we bound, captured at bind time.
        /// Drop only `remove_file` if the path still resolves to the same
        /// inode — otherwise another process has rebound the path and the
        /// file at it belongs to them.
        bound_inode: Option<u64>,
    }

    /// Connected stream — both halves go through the same socket.
    pub type StreamPair = UnixStream;

    impl Listener {
        /// Bind a listener at the given endpoint. UDS only on Unix; passing
        /// `Endpoint::NamedPipe` returns an [`IpcError::Io`] with
        /// `Unsupported` so the caller's branch logic stays simple.
        pub async fn bind(ep: &Endpoint) -> Result<Self, IpcError> {
            let path = match ep {
                Endpoint::Uds(p) => p.clone(),
                Endpoint::NamedPipe(_) => {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "named pipes are not available on Unix",
                    )))
                }
            };
            // Drop a stale socket file from a previous run. We only remove
            // the path if it is currently a socket type — files that happen
            // to live at the same path (created by a misconfigured caller)
            // are left alone, and so are directories. Symlinks are NOT
            // followed: `symlink_metadata` reports the link itself, so a
            // same-UID attacker can't redirect us into deleting an
            // unrelated file by planting a symlink at our path.
            use std::os::unix::fs::FileTypeExt as _;
            if let Ok(meta) = tokio::fs::symlink_metadata(&path).await {
                if meta.file_type().is_socket() {
                    let _ = tokio::fs::remove_file(&path).await;
                }
            }
            let inner = bind_with_restrictive_umask(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            // Capture the inode of the file we just created so Drop can
            // verify it before unlinking.
            let bound_inode = std::fs::symlink_metadata(&path).ok().map(|m| {
                use std::os::unix::fs::MetadataExt as _;
                m.ino()
            });
            Ok(Self {
                inner,
                path,
                bound_inode,
            })
        }

        /// Accept one connection.
        pub async fn accept(&self) -> Result<StreamPair, IpcError> {
            let (stream, _addr) = self.inner.accept().await?;
            verify_peer_uid(&stream)?;
            Ok(stream)
        }

        /// Path of the socket file (Unix only).
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Best-effort cleanup, with an inode check to avoid deleting
            // a socket file that now belongs to a different daemon (e.g.
            // a fast restart bound the same path between our bind and our
            // drop). Ignore "doesn't exist" / stat failures.
            use std::os::unix::fs::MetadataExt as _;
            if let (Some(want), Ok(meta)) =
                (self.bound_inode, std::fs::symlink_metadata(&self.path))
            {
                if meta.ino() != want {
                    return;
                }
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Connect a client to the given endpoint.
    pub async fn connect(ep: &Endpoint) -> Result<StreamPair, IpcError> {
        let path = match ep {
            Endpoint::Uds(p) => p,
            Endpoint::NamedPipe(_) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "named pipes are not available on Unix",
                )))
            }
        };
        let s = UnixStream::connect(path).await?;
        Ok(s)
    }

    fn bind_with_restrictive_umask(path: &Path) -> io::Result<UnixListener> {
        // SAFETY: umask is process-global, so keep the critical section to the
        // single bind syscall and always restore the previous value.
        let old = unsafe { libc::umask(0o077) };
        let result = UnixListener::bind(path);
        unsafe {
            libc::umask(old);
        }
        result
    }

    fn verify_peer_uid(stream: &UnixStream) -> io::Result<()> {
        let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        let cred = unsafe { cred.assume_init() };
        let expected = unsafe { libc::geteuid() };
        if cred.uid != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("peer uid {} does not match daemon uid {expected}", cred.uid),
            ));
        }
        Ok(())
    }

    /// Convenience: socket-file mode-checking for tests. Returns the file
    /// permissions bits (Unix only).
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn socket_mode(path: &Path) -> std::io::Result<u32> {
        let meta = std::fs::metadata(path)?;
        Ok(meta.permissions().mode())
    }
}

// ---------------- Windows implementation ----------------

#[cfg(windows)]
mod imp {
    use super::*;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// Listener handle. Named Pipes don't have a long-lived listener like
    /// UDS; each instance is created on demand via `ServerOptions`.
    pub struct Listener {
        name: String,
        next: tokio::sync::Mutex<NamedPipeServer>,
    }

    /// Stream type used by both server-accepted and client-connected sides.
    pub enum StreamPair {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    impl tokio::io::AsyncRead for StreamPair {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                StreamPair::Server(s) => std::pin::Pin::new(s).poll_read(cx, buf),
                StreamPair::Client(c) => std::pin::Pin::new(c).poll_read(cx, buf),
            }
        }
    }

    impl tokio::io::AsyncWrite for StreamPair {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            match self.get_mut() {
                StreamPair::Server(s) => std::pin::Pin::new(s).poll_write(cx, buf),
                StreamPair::Client(c) => std::pin::Pin::new(c).poll_write(cx, buf),
            }
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                StreamPair::Server(s) => std::pin::Pin::new(s).poll_flush(cx),
                StreamPair::Client(c) => std::pin::Pin::new(c).poll_flush(cx),
            }
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                StreamPair::Server(s) => std::pin::Pin::new(s).poll_shutdown(cx),
                StreamPair::Client(c) => std::pin::Pin::new(c).poll_shutdown(cx),
            }
        }
    }

    impl Listener {
        pub async fn bind(ep: &Endpoint) -> Result<Self, IpcError> {
            let name = match ep {
                Endpoint::NamedPipe(n) => n.clone(),
                Endpoint::Uds(_) => {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "UDS is not available on Windows",
                    )))
                }
            };
            // `first_pipe_instance(true)` enforces that this is the first
            // instance — the documented Windows pattern for "I am the server".
            let first = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            Ok(Self {
                name,
                next: tokio::sync::Mutex::new(first),
            })
        }

        pub async fn accept(&self) -> Result<StreamPair, IpcError> {
            // Hand out the pre-created instance, then immediately create the
            // next one so a second client doesn't race in to a closed pipe.
            let mut slot = self.next.lock().await;
            let new_next = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            let server = std::mem::replace(&mut *slot, new_next);
            drop(slot);
            server.connect().await?;
            Ok(StreamPair::Server(server))
        }

        pub fn name(&self) -> &str {
            &self.name
        }
    }

    pub async fn connect(ep: &Endpoint) -> Result<StreamPair, IpcError> {
        let name = match ep {
            Endpoint::NamedPipe(n) => n,
            Endpoint::Uds(_) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "UDS is not available on Windows",
                )))
            }
        };
        // Bounded retry loop for ERROR_PIPE_BUSY (Win32 OS error 231): the
        // listener only has one pre-listening server instance at a time, so
        // concurrent client opens that arrive before the listener task
        // finishes swapping in the next instance get a busy. We retry for
        // up to 5 s with a 20 ms backoff — well inside the daemon's normal
        // accept-loop turnaround.
        const ERROR_PIPE_BUSY: i32 = 231;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match ClientOptions::new().open(name) {
                Ok(c) => return Ok(StreamPair::Client(c)),
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub use imp::*;
