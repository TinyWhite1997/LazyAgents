//! Cross-platform IPC transport.
//!
//! On Unix we use `tokio::net::UnixListener` / `UnixStream` (the daemon binds
//! a socket file under `$XDG_RUNTIME_DIR/lazyagents/lad.sock` in production;
//! tests create one under `tempfile`). On Windows we use Named Pipes via
//! `tokio::net::windows::named_pipe`. The two platforms share the
//! [`StreamPair`] alias so callers can be transport-agnostic above this layer.
//!
//! Security note: Unix listeners bind under a temporary `umask(0o077)`, then
//! chmod the socket file to 0600 and verify accepted peers with platform peer
//! credentials (`SO_PEERCRED` on Linux/Android, `getpeereid` on BSD-family
//! targets). Windows listeners (WEK-81) create every pipe instance with an
//! owner-only DACL built from the current process's user SID (SDDL
//! `D:P(A;;GA;;;<owner-sid>)(A;;GA;;;SY)`) so other local users in the same
//! interactive session cannot open the pipe; they also call
//! `reject_remote_clients(true)` to block SMB hops and re-verify the
//! accepted peer's process-token SID against the daemon's cached owner SID
//! as defense in depth.
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
        let peer_uid = peer_uid(stream)?;
        let expected = unsafe { libc::geteuid() };
        if peer_uid != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("peer uid {peer_uid} does not match daemon uid {expected}"),
            ));
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
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
        Ok(cred.uid)
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    fn peer_uid(_stream: &UnixStream) -> io::Result<libc::uid_t> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix peer credential validation is not implemented on this target",
        ))
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
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        CopySid, EqualSid, GetLengthSid, GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // ---------- RAII for HANDLEs and LocalAlloc'd buffers ----------

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalAllocPtr(*mut c_void);
    impl Drop for LocalAllocPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0 as HLOCAL);
                }
            }
        }
    }

    /// Owned copy of the current process's user SID. Used both as the DACL
    /// principal at create time and as the trusted-peer SID at accept time.
    #[derive(Clone)]
    struct OwnerSid {
        bytes: Vec<u8>,
    }

    impl OwnerSid {
        fn as_psid(&self) -> PSID {
            self.bytes.as_ptr() as PSID
        }
    }

    /// Pull the current process's user SID via OpenProcessToken +
    /// GetTokenInformation(TokenUser) + CopySid. Two-call pattern because
    /// `GetTokenInformation` always fails with INSUFFICIENT_BUFFER on the
    /// sizing probe and writes the required length back through the last
    /// parameter (verified against MSDN + windows-sys 0.59 signature).
    fn current_user_sid() -> std::io::Result<OwnerSid> {
        // SAFETY: pure Win32 FFI; every HANDLE / LocalAlloc'd buffer is
        // bounded by an RAII drop guard.
        unsafe {
            let mut tok: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _tok_guard = OwnedHandle(tok);

            let mut need: u32 = 0;
            // Sizing probe: ignore the (always failing) return; rely on `need`.
            let _ = GetTokenInformation(tok, TokenUser, ptr::null_mut(), 0, &mut need);
            let mut buf = vec![0u8; need as usize];
            if GetTokenInformation(
                tok,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                need,
                &mut need,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let tu = buf.as_ptr() as *const TOKEN_USER;
            let psid = (*tu).User.Sid;

            // CopySid: serialize the SID into a standalone buffer so callers
            // can keep it past `buf`'s drop. SIDs are ~28-68 bytes; the
            // alloc is cheap.
            let len = GetLengthSid(psid);
            let mut sid_buf = vec![0u8; len as usize];
            if CopySid(len, sid_buf.as_mut_ptr() as PSID, psid) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(OwnerSid { bytes: sid_buf })
        }
    }

    /// Build the wide SDDL string `D:P(A;;GA;;;<sid>)(A;;GA;;;SY)`:
    /// Protected DACL (no inheritance) granting GENERIC_ALL to the owner
    /// SID and to LocalSystem only.
    fn sddl_for_owner(owner: &OwnerSid) -> std::io::Result<Vec<u16>> {
        unsafe {
            let mut sid_str_ptr: *mut u16 = ptr::null_mut();
            if ConvertSidToStringSidW(owner.as_psid(), &mut sid_str_ptr) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Wrap immediately so an early-return frees the LocalAlloc'd buf.
            let _free = LocalAllocPtr(sid_str_ptr as *mut c_void);

            let mut len = 0usize;
            while *sid_str_ptr.add(len) != 0 {
                len += 1;
            }
            let sid_slice = std::slice::from_raw_parts(sid_str_ptr, len);
            let sid_str = String::from_utf16(sid_slice).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf16 SID string")
            })?;

            let sddl = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)", sid = sid_str);
            let mut wide: Vec<u16> = sddl.encode_utf16().collect();
            wide.push(0);
            Ok(wide)
        }
    }

    /// SECURITY_ATTRIBUTES + the SECURITY_DESCRIPTOR it points at. The
    /// kernel deep-copies the SD into the pipe object inside
    /// `CreateNamedPipeW`, so this struct only needs to outlive the
    /// `create_with_security_attributes_raw` call — Drop frees the
    /// LocalAlloc'd descriptor.
    struct OwnerOnlySa {
        sa: SECURITY_ATTRIBUTES,
        _sd: LocalAllocPtr,
    }

    impl OwnerOnlySa {
        fn new(owner: &OwnerSid) -> std::io::Result<Self> {
            let sddl_w = sddl_for_owner(owner)?;
            let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl_w.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let sd_guard = LocalAllocPtr(psd);
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd,
                bInheritHandle: 0,
            };
            Ok(OwnerOnlySa { sa, _sd: sd_guard })
        }

        fn as_ptr(&self) -> *mut c_void {
            &self.sa as *const _ as *mut c_void
        }
    }

    /// Single-source helper so `bind` and `accept` cannot drift apart:
    /// both server-instance creation sites MUST route through this so the
    /// second-and-later instances stay owner-locked too. Otherwise the
    /// first connection is safe but every subsequent one falls back to
    /// the default DACL (which allows other interactive-session users).
    fn create_locked_server(
        opts: &ServerOptions,
        name: &str,
        owner: &OwnerSid,
    ) -> std::io::Result<NamedPipeServer> {
        let sa = OwnerOnlySa::new(owner)?;
        // SAFETY: `sa.as_ptr()` is valid for this call; the kernel copies
        // the SD into the pipe object before returning per
        // CreateNamedPipeW's contract.
        unsafe { opts.create_with_security_attributes_raw(name, sa.as_ptr()) }
    }

    /// Belt-and-suspenders peer check: after `connect().await`, look up
    /// the client process's user SID via GetNamedPipeClientProcessId +
    /// OpenProcessToken + GetTokenInformation(TokenUser) and compare to
    /// the cached owner SID. Prefers this read-only path over
    /// `ImpersonateNamedPipeClient`, which requires SE_IMPERSONATE_NAME
    /// (often absent for a plain-user daemon) and mutates thread-local
    /// security context.
    fn verify_peer_is_owner(
        server: &NamedPipeServer,
        expected_owner: &OwnerSid,
    ) -> std::io::Result<()> {
        unsafe {
            let raw = server.as_raw_handle() as HANDLE;
            let mut peer_pid: u32 = 0;
            if GetNamedPipeClientProcessId(raw, &mut peer_pid) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let proc_h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, peer_pid);
            if proc_h.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let _proc_guard = OwnedHandle(proc_h);

            let mut tok: HANDLE = ptr::null_mut();
            if OpenProcessToken(proc_h, TOKEN_QUERY, &mut tok) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _tok_guard = OwnedHandle(tok);

            let mut need: u32 = 0;
            let _ = GetTokenInformation(tok, TokenUser, ptr::null_mut(), 0, &mut need);
            let mut buf = vec![0u8; need as usize];
            if GetTokenInformation(
                tok,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                need,
                &mut need,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let tu = buf.as_ptr() as *const TOKEN_USER;
            let peer_sid = (*tu).User.Sid;

            if EqualSid(peer_sid, expected_owner.as_psid()) == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("named pipe peer pid {peer_pid} SID does not match server owner"),
                ));
            }
            Ok(())
        }
    }

    /// Listener handle. Named Pipes don't have a long-lived listener like
    /// UDS; each instance is created on demand via `ServerOptions`. The
    /// cached `owner` SID is the principal both the DACL and the
    /// peer-verification step compare against — capture once at bind
    /// time so a forked daemon would not silently relax the contract.
    pub struct Listener {
        name: String,
        owner: OwnerSid,
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
            let owner = current_user_sid().map_err(IpcError::Io)?;
            // `first_pipe_instance(true)` enforces that this is the first
            // instance — the documented Windows pattern for "I am the server".
            // It can only be set on the very first ServerOptions creation —
            // a second instance with the flag fails EACCES (already exists).
            let mut opts = ServerOptions::new();
            opts.first_pipe_instance(true).reject_remote_clients(true);
            let first = create_locked_server(&opts, &name, &owner).map_err(IpcError::Io)?;
            Ok(Self {
                name,
                owner,
                next: tokio::sync::Mutex::new(first),
            })
        }

        pub async fn accept(&self) -> Result<StreamPair, IpcError> {
            // Hand out the pre-created instance, then immediately create the
            // next one (also owner-locked!) so a second client doesn't race
            // in to a closed pipe.
            let mut slot = self.next.lock().await;
            let mut opts = ServerOptions::new();
            opts.reject_remote_clients(true);
            let new_next =
                create_locked_server(&opts, &self.name, &self.owner).map_err(IpcError::Io)?;
            let server = std::mem::replace(&mut *slot, new_next);
            drop(slot);
            server.connect().await?;

            // Belt-and-suspenders: even though the DACL above gates
            // CreateFileW at the kernel, also verify the connected peer's
            // process-token SID matches our owner SID. Defends against ACL
            // misconfiguration / kernel-level regressions / future refactors
            // that accidentally relax `create_locked_server`.
            if let Err(e) = verify_peer_is_owner(&server, &self.owner) {
                tracing::warn!(error = %e, "rejecting named-pipe peer: SID mismatch");
                drop(server);
                return Err(IpcError::Io(e));
            }
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
        // accept-loop turnaround. Cross-user opens hit ERROR_ACCESS_DENIED
        // (5) *before* BUSY because `CreateFileW` evaluates the DACL first,
        // so this loop is not a denial-of-service vector.
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
