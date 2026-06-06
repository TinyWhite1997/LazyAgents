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
/// stem is short and globally unique so concurrent daemons can't
/// collide on the shared `\\.\pipe\lazyagents-<stem>` namespace. We
/// embed `la_storage::new_id()` (ULID-grade) on top of the
/// process / per-binary counter pair so two cargo-spawned test
/// binaries that happen to share a PID (cargo runs each binary with
/// its own counter starting at 0) still get distinct pipe names.
/// On Unix the tempdir is already unique per call AND `bind(2)`
/// enforces `SUN_LEN` (104 bytes on macOS) — keep the canonical
/// `lad-1.sock` name so the path stays well under the limit.
pub fn unique_socket_path(runtime_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // `new_id` is a ULID — 26 chars, lowercase. Truncate to keep
        // the resulting `\\.\pipe\lazyagents-lad-test-<pid>-<n>-<id>`
        // name comfortably under Windows's pipe-name length cap.
        let id = la_storage::new_id();
        let suffix: String = id.chars().take(10).collect();
        let stem = format!(
            "lad-test-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            suffix,
        );
        runtime_dir.join(format!("{stem}.sock"))
    }
    #[cfg(not(windows))]
    {
        runtime_dir.join("lad-1.sock")
    }
}
