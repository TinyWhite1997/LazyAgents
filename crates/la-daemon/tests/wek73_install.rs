//! WEK-73 / M4.1 — install/uninstall round-trip test.
//!
//! Exercises the systemd install controller end-to-end with a fake
//! `$HOME` so it writes into a tempdir. The `systemctl --user`
//! invocations the controller makes are best-effort (this CI host has
//! no user systemd instance, and that's fine); the test cares about
//! the unit-file lifecycle, not the service manager itself:
//!
//! * `install` writes the unit file with `LAZYAGENTS_MANAGED_BY=systemd`.
//! * `install` is idempotent (second run returns `Already`).
//! * `uninstall` removes the file and is idempotent.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use la_daemon::install::actions::ActionOutcome;
use la_daemon::install::systemd::SystemdController;
use la_daemon::install::{InstallContext, ServiceController};

fn fake_ctx(home: &std::path::Path) -> InstallContext {
    InstallContext {
        exec_path: PathBuf::from("/usr/local/bin/lad"),
        config_path: home.join(".config/lazyagents/config.toml"),
        home: home.to_path_buf(),
        user: "tester".to_string(),
        dry_run: false,
    }
}

#[test]
fn systemd_install_uninstall_roundtrip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();

    // Hide any host XDG_CONFIG_HOME so the unit path is purely
    // `<home>/.config/systemd/user/lad.service`.
    let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_CONFIG_HOME");

    let ctx = fake_ctx(home);
    let ctrl = SystemdController::from_ctx(&ctx);
    let unit_path = ctrl.paths.unit_path.clone();

    // Install — file exists with expected substitutions.
    let first = ctrl.install(&ctx).expect("install");
    assert!(matches!(first, ActionOutcome::Done { .. }), "{first}");
    assert!(unit_path.exists(), "unit file should exist after install");
    let body = std::fs::read_to_string(&unit_path).expect("read unit");
    assert!(body.contains("ExecStart=/usr/local/bin/lad start --config"));
    assert!(body.contains("LAZYAGENTS_MANAGED_BY=systemd"));
    assert!(!body.contains("{{"));

    // Re-install with identical inputs — idempotent (Already).
    let second = ctrl.install(&ctx).expect("install#2");
    assert!(
        matches!(second, ActionOutcome::Already { .. }),
        "second install should be Already, got {second}"
    );

    // Uninstall — file removed; verb returns Done.
    let third = ctrl.uninstall(&ctx).expect("uninstall");
    assert!(matches!(third, ActionOutcome::Done { .. }), "{third}");
    assert!(!unit_path.exists(), "unit file should be removed");

    // Uninstall again — Already (nothing to remove).
    let fourth = ctrl.uninstall(&ctx).expect("uninstall#2");
    assert!(
        matches!(fourth, ActionOutcome::Already { .. }),
        "second uninstall should be Already, got {fourth}"
    );

    if let Some(prev) = prev_xdg {
        std::env::set_var("XDG_CONFIG_HOME", prev);
    }
}

#[test]
fn systemd_install_dry_run_writes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_CONFIG_HOME");

    let mut ctx = fake_ctx(home);
    ctx.dry_run = true;
    let ctrl = SystemdController::from_ctx(&ctx);

    let outcome = ctrl.install(&ctx).expect("dry-run install");
    assert!(matches!(outcome, ActionOutcome::Done { .. }));
    assert!(
        !ctrl.paths.unit_path.exists(),
        "dry-run must not touch the filesystem"
    );

    if let Some(prev) = prev_xdg {
        std::env::set_var("XDG_CONFIG_HOME", prev);
    }
}
