//! Back-compat alias module for the pre-WEK-36 `health_sub` API.
//!
//! The functionality moved to [`crate::notif_sub`] in WEK-36 (status bar
//! gained `cron.fired` + auto-reconnect on top of the original
//! `daemon.health` pump). This file keeps the original names exported so
//! out-of-crate call sites (the `la` binary in particular) keep
//! compiling; new code should reach for `notif_sub` directly.

pub use crate::notif_sub::{spawn, spawn_with_config, HealthEvent, NotifEvent, ReconnectConfig};
