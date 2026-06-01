//! Cross-platform PTY abstraction for LazyAgents.
//!
//! Wraps [`portable_pty`] and exposes a tokio-friendly handle:
//! [`PtyChild`] with a `reader` (mpsc receiver of bytes), a [`PtyWriter`],
//! and methods for `resize`, `signal`, and `wait`.
//!
//! See `README.md` for the cross-platform behavior table and limitations.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::mpsc;

pub use portable_pty::{CommandBuilder, ExitStatus};

mod platform;

/// Send a cross-platform signal to a pid that this crate previously spawned.
///
/// Equivalent to [`PtyChild::signal`] but addressable by pid, so a daemon
/// can detach the child from its [`PtyChild`] handle (e.g. when the
/// session-manager task owns the handle while a `sessions.signal` RPC is
/// dispatched on a different task) and still target it.
///
/// Pid lookup is the OS's responsibility; on Unix this means the pid must
/// still belong to a process in our process group (the child is `setsid`'d
/// at spawn time so pgid == pid). On Windows the pid must still be a
/// console-group leader created with `CREATE_NEW_PROCESS_GROUP` — which
/// `portable-pty` does for us. After the child reaps, the call returns
/// `Err(PtyError::Signal(_))` instead of silently no-op'ing.
pub fn send_signal(pid: u32, sig: Signal) -> Result<(), PtyError> {
    platform::send_signal(pid, sig)
}

/// Initial / resized PTY window size.
#[derive(Debug, Clone, Copy)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl From<PtySize> for portable_pty::PtySize {
    fn from(s: PtySize) -> Self {
        portable_pty::PtySize {
            rows: s.rows,
            cols: s.cols,
            pixel_width: s.pixel_width,
            pixel_height: s.pixel_height,
        }
    }
}

/// Cross-platform signal mapped to the appropriate native action.
///
/// See [`crate::platform`] for per-OS semantics — notably, on Windows
/// `Interrupt`/`Terminate` go through `GenerateConsoleCtrlEvent` and
/// require the child to be in its own process group (see README).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Ctrl-C — SIGINT on Unix, CTRL_C_EVENT on Windows.
    Interrupt,
    /// Polite termination — SIGTERM on Unix, CTRL_BREAK_EVENT on Windows.
    Terminate,
    /// Forceful kill — SIGKILL on Unix, TerminateProcess on Windows.
    Kill,
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("openpty failed: {0}")]
    Open(String),
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("clone reader failed: {0}")]
    Reader(String),
    #[error("take writer failed: {0}")]
    Writer(String),
    #[error("resize failed: {0}")]
    Resize(String),
    #[error("signal failed: {0}")]
    Signal(String),
    #[error("wait failed: {0}")]
    Wait(String),
    #[error("writer half closed")]
    WriterClosed,
    #[error("child has no pid")]
    NoPid,
    #[error("join error: {0}")]
    Join(String),
}

/// Async handle to write bytes into the PTY master.
///
/// Internally backed by a dedicated blocking thread that owns the
/// `portable_pty` writer; calls here just push `Bytes` onto a bounded
/// channel. Backpressure: `write` awaits when the channel is full.
#[derive(Clone)]
pub struct PtyWriter {
    tx: mpsc::Sender<Bytes>,
}

impl PtyWriter {
    pub async fn write<B: Into<Bytes>>(&self, data: B) -> Result<(), PtyError> {
        self.tx
            .send(data.into())
            .await
            .map_err(|_| PtyError::WriterClosed)
    }

    pub fn try_write<B: Into<Bytes>>(&self, data: B) -> Result<(), PtyError> {
        self.tx
            .try_send(data.into())
            .map_err(|_| PtyError::WriterClosed)
    }
}

/// A spawned child process attached to a PTY.
///
/// Fields:
/// - [`reader`](Self::reader): bounded `mpsc::Receiver<Bytes>` carrying PTY
///   stdout/stderr (merged on the slave side). Closed on EOF.
/// - [`writer`](Self::writer): [`PtyWriter`] for input.
///
/// Methods cover resize, signal, and (consuming) wait. See
/// [`spawn`] for construction.
pub struct PtyChild {
    pid: Option<u32>,
    /// PTY output stream; receives `None` on EOF.
    pub reader: mpsc::Receiver<Bytes>,
    /// PTY input stream.
    pub writer: PtyWriter,
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtyChild {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Resize the PTY. Safe to call from any thread.
    pub fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        self.master
            .lock()
            .expect("master mutex poisoned")
            .resize(size.into())
            .map_err(|e| PtyError::Resize(e.to_string()))
    }

    /// Send a cross-platform signal. See [`Signal`] for per-OS mapping.
    pub fn signal(&self, sig: Signal) -> Result<(), PtyError> {
        let pid = self.pid.ok_or(PtyError::NoPid)?;
        platform::send_signal(pid, sig)
    }

    /// Wait for the child to exit. Consumes the handle.
    pub async fn wait(mut self) -> Result<ExitStatus, PtyError> {
        tokio::task::spawn_blocking(move || self.child.wait())
            .await
            .map_err(|e| PtyError::Join(e.to_string()))?
            .map_err(|e| PtyError::Wait(e.to_string()))
    }

    /// Split into independently-owned parts so the daemon can move the
    /// reader to one task while a second task awaits exit and a third
    /// holds the writer.
    ///
    /// After this call the child can no longer be addressed through
    /// `Self::signal` — the daemon must use [`crate::send_signal`] with
    /// the pid returned in [`PtyChildParts::pid`].
    pub fn into_parts(self) -> PtyChildParts {
        PtyChildParts {
            pid: self.pid,
            reader: self.reader,
            writer: self.writer,
            waiter: ChildWaiter {
                master: self.master,
                child: self.child,
            },
        }
    }
}

/// Result of [`PtyChild::into_parts`]; lets the caller split ownership
/// across tasks without giving up resize / wait access.
pub struct PtyChildParts {
    pub pid: Option<u32>,
    pub reader: mpsc::Receiver<Bytes>,
    pub writer: PtyWriter,
    pub waiter: ChildWaiter,
}

/// Independently-ownable half of [`PtyChild`] that retains resize / wait
/// capability after the reader and writer have been moved elsewhere.
pub struct ChildWaiter {
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl ChildWaiter {
    /// Resize the PTY; safe to call concurrently with [`Self::wait`]
    /// because the master is behind a `Mutex`.
    pub fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        self.master
            .lock()
            .expect("master mutex poisoned")
            .resize(size.into())
            .map_err(|e| PtyError::Resize(e.to_string()))
    }

    /// Wait for the child to exit. Consumes the handle.
    pub async fn wait(mut self) -> Result<ExitStatus, PtyError> {
        tokio::task::spawn_blocking(move || self.child.wait())
            .await
            .map_err(|e| PtyError::Join(e.to_string()))?
            .map_err(|e| PtyError::Wait(e.to_string()))
    }
}

/// Spawn a command attached to a freshly opened PTY.
///
/// Wires up the §6.2 read loop (`spawn_blocking` + `mpsc` backpressure):
/// when the consumer drops `reader` or stops draining, the OS-side write
/// to the PTY will eventually block, exerting flow control on the child.
pub fn spawn(cmd: CommandBuilder, size: PtySize) -> Result<PtyChild, PtyError> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(size.into())
        .map_err(|e| PtyError::Open(e.to_string()))?;

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| PtyError::Spawn(e.to_string()))?;
    // After spawn the child owns the slave fd; we drop our handle so the
    // PTY closes when the child exits (otherwise read() never sees EOF).
    drop(pair.slave);

    let pid = child.process_id();

    let master = pair.master;
    let mut reader = master
        .try_clone_reader()
        .map_err(|e| PtyError::Reader(e.to_string()))?;
    let mut writer_sync = master
        .take_writer()
        .map_err(|e| PtyError::Writer(e.to_string()))?;
    let master = Arc::new(Mutex::new(master));

    // §6.2 read loop. Uses a dedicated OS thread (not the tokio blocking
    // pool) so it never starves under burst load. `blocking_send` propagates
    // backpressure all the way back to the child via the OS PTY buffer.
    let (out_tx, out_rx) = mpsc::channel::<Bytes>(1024);
    std::thread::Builder::new()
        .name(format!("la-pty-read-{}", pid.unwrap_or(0)))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        if out_tx.blocking_send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| PtyError::Reader(e.to_string()))?;

    // Symmetric write loop: a dedicated thread drains an mpsc into the
    // sync writer. Decouples async callers from blocking writes.
    let (in_tx, mut in_rx) = mpsc::channel::<Bytes>(1024);
    std::thread::Builder::new()
        .name(format!("la-pty-write-{}", pid.unwrap_or(0)))
        .spawn(move || {
            use std::io::Write;
            while let Some(bytes) = in_rx.blocking_recv() {
                if writer_sync.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer_sync.flush();
            }
        })
        .map_err(|e| PtyError::Writer(e.to_string()))?;

    Ok(PtyChild {
        pid,
        reader: out_rx,
        writer: PtyWriter { tx: in_tx },
        master,
        child,
    })
}
