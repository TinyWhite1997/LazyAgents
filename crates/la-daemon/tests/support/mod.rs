//! Cross-platform test support for la-daemon integration tests.
//!
//! WEK-84 — every test daemon in this crate is bootstrapped from a
//! `runtime_dir` (tempdir) + a socket-shaped path. On Unix the socket
//! lives directly on disk; on Windows `la_ipc::transport::endpoint_for`
//! derives the Named Pipe name from the path's file stem. Two parallel
//! test daemons that both pick the canonical `lad-1.sock` would race
//! for the same `\\.\pipe\lazyagents-lad-1` on Windows. Mirror the
//! `integration/m2-smoke` convention here so the daemon-test harness
//! follows the same single source of truth as the m2-smoke harness.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Build a socket-shaped path under `runtime_dir`. On Windows the file
/// stem is short and process-/counter-unique so concurrent daemons
/// don't collide on the shared `\\.\pipe\lazyagents-<stem>` namespace.
/// On Unix the tempdir is already unique per call AND `bind(2)` enforces
/// `SUN_LEN` (104 bytes on macOS) — keep the canonical `lad-1.sock` name
/// so the path stays well under the limit.
pub fn unique_socket_path(runtime_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let stem = format!(
            "lad-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        runtime_dir.join(format!("{stem}.sock"))
    }
    #[cfg(not(windows))]
    {
        runtime_dir.join("lad-1.sock")
    }
}
