//! Process-level signal handling for graceful shutdown.
//!
//! Architecture §6.4 calls for a graceful-stop sequence with a 10 s hard
//! cap: on SIGINT or SIGTERM the daemon stops accepting new connections,
//! tells every live session to wind down (in-band exit → SIGTERM → SIGKILL),
//! flushes pending storage writes, and only then exits.
//!
//! This module gives the [`runtime`](crate::runtime) module a
//! transport-agnostic [`shutdown_token`] future that resolves once any
//! of the registered signals fires. The runtime composes the future with
//! its own per-session teardown logic; the signal handler itself does no
//! business work — it just nudges the runtime out of `accept().await`.

use std::time::Duration;

use tokio::signal::ctrl_c;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

/// Default hard cap on graceful shutdown — sessions that don't exit
/// within this window are SIGKILLed. Mirrors §6.4 ("整个序列在 daemon
/// 关闭时对所有 session 并发执行，硬超时 10 s").
pub const DEFAULT_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);

/// Future that resolves the first time SIGINT, SIGTERM, or (Windows)
/// Ctrl-C is observed. Spawning more than one waiter per process is
/// supported — each independently consumes its own copy.
///
/// On Unix the signals are routed through `tokio::signal::unix`, which
/// uses an `eventfd`-backed handler so the future is signal-safe.
pub async fn shutdown_token() {
    #[cfg(unix)]
    {
        // Install both handlers concurrently; the first to fire wins.
        let term = async {
            if let Ok(mut sig) = signal(SignalKind::terminate()) {
                let _ = sig.recv().await;
            } else {
                // If we can't install the handler (very rare; almost
                // always means the process started without a signal mask),
                // fall back to a never-resolving future so SIGINT alone
                // still works.
                std::future::pending::<()>().await;
            }
        };
        let int = async {
            if let Ok(mut sig) = signal(SignalKind::interrupt()) {
                let _ = sig.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = term => {},
            _ = int => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c().await;
    }
    // Make `ctrl_c` visible to non-unix builds too — avoids an unused
    // import warning under cfg(unix).
    #[cfg(unix)]
    {
        let _ = ctrl_c;
    }
}
