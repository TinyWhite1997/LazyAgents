//! `la-daemon` — assembles `la-core` + `la-storage` + `la-ipc` into the
//! `lad` daemon binary.
//!
//! Implements **WEK-21 / M1.7** from `report/技术架构设计.md`:
//!
//! - **§2 / §2.1**: `lad` is the composition layer that wires the
//!   `SessionManager` (la-core) on top of `Storage` (la-storage) and an
//!   `OutputHub`-backed IPC listener (la-ipc). The TUI talks to it across
//!   the JSON-RPC envelope from la-proto; no other crate links la-core.
//! - **§11.2 启动模式**: a `lad start` foreground mode and a `lad daemonize`
//!   fork-and-detach mode. `la` uses the same socket-path discovery and
//!   auto-spawns the daemonize variant when it cannot connect.
//! - **§11.3 多版本共存**: socket paths embed the protocol major
//!   (`lad-1.sock`) so a future v2 daemon co-exists with v1 on the same
//!   host.
//! - **§6.4 优雅停机**: SIGINT / SIGTERM enter a graceful shutdown that
//!   gives running children up to 10 s to clean up before the listener
//!   tears down and pending RPCs are cancelled.
//!
//! The crate exposes a small library surface (`Daemon`, `paths`, `signals`,
//! `dispatcher`) plus the `lad` binary at `src/bin/lad.rs`. The library
//! split lets integration tests drive a daemon in-process via [`Daemon`]
//! without going through the binary's CLI parser.

pub mod config_cmd;
pub mod cron_security;
pub mod daemonize;
pub mod dispatcher;
pub mod health;
pub mod paths;
pub mod runtime;
pub mod scheduler;
pub mod signals;

pub use daemonize::{spawn_daemonized, DaemonizeError};
pub use dispatcher::AdapterRegistry;
pub use health::{HealthRegistry, ProbeLoopConfig, DEFAULT_PROBE_INTERVAL};
pub use paths::{default_socket_path, default_state_dir, socket_path_for_version, SocketDiscovery};
pub use runtime::{metrics_socket_path, Daemon, DaemonConfig, DaemonError, DaemonHandle};
pub use scheduler::{SchedulerConfig, SchedulerServices};

/// Daemon-advertised name on the wire (`InitializeResult.server`).
pub const SERVER_NAME: &str = "lad";

/// Daemon binary version reported on the wire. Tracked separately from the
/// crate `CARGO_PKG_VERSION` so library consumers can build daemons with a
/// custom string (tests pin a stable value; production reads `env!`).
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
