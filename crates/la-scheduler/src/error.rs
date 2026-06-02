//! Scheduler error type. Variants line up with the `CRON_*` IPC codes in
//! [`la_proto::error_codes`] — the daemon's `to_rpc_error` mapper turns each
//! variant into the documented wire code without exposing internal detail
//! (architecture §9.1).
//!
//! We deliberately do **not** depend on `la-proto` from this crate to keep
//! the dependency graph one-way (proto -> consumers, scheduler is a leaf).
//! The la-daemon dispatcher layer holds the mapping table.

/// Scheduler-side classification of failure. Crosses crate boundaries; the
/// wire mapping lives in la-daemon.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Cron expression failed to parse (5/6 field check or upstream cron
    /// crate refused). Maps to `CRON_INVALID_EXPR` (-33302).
    #[error("invalid cron expression {raw:?}: {reason}")]
    InvalidExpr { raw: String, reason: String },

    /// IANA timezone name not recognised by chrono-tz. Maps to
    /// `CRON_INVALID_TZ` (-33304).
    #[error("invalid IANA timezone {0:?}")]
    InvalidTimezone(String),

    /// Caller asked to upsert/delete an id with malformed metadata. Maps to
    /// generic internal error since these are programmer-side bugs, not user
    /// input — surfacing them sharply during integration catches wiring
    /// mistakes.
    #[error("scheduler invariant violated: {0}")]
    Invariant(&'static str),
}
