//! Wire contracts for the LazyAgents daemon ↔ client protocol.
//!
//! This crate intentionally has **no** transport dependencies: it only defines
//! the JSON-RPC 2.0 envelope and the M1 method/notification surface from
//! architecture §3 (full `sessions.*` table, `events.subscribe`, and every
//! server-pushed notification). Transport (UDS / Named Pipe, length-prefix
//! framing) lives in `la-ipc`.
//!
//! ## Layout
//!
//! - [`jsonrpc`] — generic JSON-RPC 2.0 envelopes ([`jsonrpc::Request`],
//!   [`jsonrpc::Response`], [`jsonrpc::Notification`], [`jsonrpc::RpcError`]).
//!   Method names and params are erased to `String` / `serde_json::Value`
//!   here so a single decode call can dispatch to typed payloads in
//!   [`methods`] / [`notifications`].
//! - [`methods`] — typed `params` / `result` for each RPC.
//! - [`notifications`] — typed payloads for server → client pushes.
//! - [`chunking`] — splits a single PTY payload into ≤64 KiB chunks for
//!   `session.output` (per architecture §3 帧格式).
//!
//! The on-wire byte budget (4 MiB max message, 4-byte BE length prefix) is
//! defined here as a constant so framing in `la-ipc` and chunking here agree.

pub mod chunking;
pub mod jsonrpc;
pub mod methods;
pub mod notifications;

use serde::Serialize;

/// Maximum size of a single JSON-RPC message on the wire (bytes).
///
/// Mirrors `la-ipc::MAX_MESSAGE_BYTES`. Defined here so producers (the chunker,
/// the typed encoders) can refuse to build payloads that the transport would
/// later reject.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Maximum payload size of a single `session.output` notification (bytes).
///
/// PTY output that exceeds this is split into multiple notifications by
/// [`chunking::chunk_session_output`].
pub const SESSION_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

/// The single supported protocol major version for M1.
///
/// Negotiated via [`methods::Initialize`]. Newer minors add fields without
/// changing this value; a breaking change bumps it and the daemon must keep
/// supporting `N-1` for one minor cycle.
pub const PROTOCOL_VERSION: &str = "1";

/// JSON-RPC error code namespace ranges (architecture §9.1).
///
/// **Layout**:
/// - `-32700 .. -32600`: JSON-RPC 2.0 reserved (parse / invalid request /
///   method-not-found / invalid-params / internal).
/// - `-32099 .. -32000`: server-implementation reserved (handshake-level
///   transport failures). Use [`to_rpc_error`] with `kind` =
///   [`ErrorKind::Server`] to allocate codes in this range.
/// - `-33000 ..`: LazyAgents business errors. Subranges below group by
///   subsystem so a glance at the code tells you where to look:
///   - `-33001 .. -33099`: protocol / session lifecycle
///   - `-33100 .. -33199`: adapter / backend
///   - `-33200 .. -33299`: storage / SQLite
///   - `-33300 .. -33399`: scheduler / cron (post-M3, codes defined now
///     so the M3 implementation doesn't accidentally collide)
pub mod error_codes {
    /// `-32700` — invalid JSON received.
    pub const PARSE_ERROR: i32 = -32700;
    /// `-32600` — JSON sent is not a valid Request object.
    pub const INVALID_REQUEST: i32 = -32600;
    /// `-32601` — method does not exist.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// `-32602` — invalid method parameter(s).
    pub const INVALID_PARAMS: i32 = -32602;
    /// `-32603` — internal JSON-RPC error.
    pub const INTERNAL_ERROR: i32 = -32603;

    /// Server-implementation error range start (`-32099`).
    pub const SERVER_ERROR_START: i32 = -32099;
    /// Server-implementation error range end (`-32000`).
    pub const SERVER_ERROR_END: i32 = -32000;

    /// LazyAgents business errors start here (architecture §9.1: `-33000..`).
    pub const BUSINESS_ERROR_START: i32 = -33000;

    // ----- Protocol / session lifecycle: -33001..-33099 -----

    /// `-33001` — `initialize` not yet called.
    pub const NOT_INITIALIZED: i32 = -33001;
    /// `-33002` — protocol version mismatch in `initialize`.
    pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -33002;
    /// `-33003` — session id does not exist.
    pub const SESSION_NOT_FOUND: i32 = -33003;
    /// `-33004` — input ownership held by another client
    /// (architecture §3 关键不变量: 单一写者).
    pub const WRITER_LOCKED: i32 = -33004;
    /// `-33005` — the session is not currently attached to this connection
    /// (e.g. `sessions.write` / `sessions.detach` before `sessions.attach`).
    pub const NOT_ATTACHED: i32 = -33005;
    /// `-33006` — ring buffer can no longer satisfy the requested range
    /// (client should accept the gap and move on).
    pub const REPLAY_OUT_OF_RANGE: i32 = -33006;
    /// `-33007` — `sessions.delete` on a session that is still running.
    pub const SESSION_BUSY: i32 = -33007;
    /// `-33008` — payload exceeds [`crate::MAX_MESSAGE_BYTES`].
    pub const PAYLOAD_TOO_LARGE: i32 = -33008;
    /// `-33009` — unknown / unsupported `EventTopic` in `events.subscribe`.
    pub const UNKNOWN_EVENT_TOPIC: i32 = -33009;

    // ----- Adapter / backend: -33100..-33199 -----

    /// `-33101` — the named backend isn't installed on this host.
    pub const ADAPTER_NOT_INSTALLED: i32 = -33101;
    /// `-33102` — the backend CLI exists but the user isn't logged in.
    pub const ADAPTER_UNAUTHENTICATED: i32 = -33102;
    /// `-33103` — spawning the backend's child process failed (IO error).
    pub const ADAPTER_SPAWN_FAILED: i32 = -33103;
    /// `-33104` — the backend's output format diverged from what the
    /// adapter parser expects (often "upgrade your CLI" territory).
    pub const ADAPTER_PROTOCOL_DRIFT: i32 = -33104;
    /// `-33105` — the request asked for a backend feature this adapter
    /// version doesn't expose (e.g. JSON output mode).
    pub const ADAPTER_UNSUPPORTED_OPTION: i32 = -33105;

    // ----- Storage / SQLite: -33200..-33299 -----

    /// `-33201` — SQLite reported `SQLITE_BUSY` after our retry budget.
    pub const STORAGE_BUSY: i32 = -33201;
    /// `-33202` — a uniqueness / FK invariant was violated; the caller
    /// most likely raced a competing writer.
    pub const STORAGE_CONFLICT: i32 = -33202;
    /// `-33203` — any other storage-level failure (corruption, disk full,
    /// IO error during migration, …). Treated as terminal — the daemon
    /// should surface a UX-level retry, not an automatic one.
    pub const STORAGE_FAILED: i32 = -33203;

    // ----- Scheduler / cron: -33300..-33399 (post-M3 surface) -----

    /// `-33301` — cron id not found.
    pub const CRON_NOT_FOUND: i32 = -33301;
    /// `-33302` — cron expression failed to parse.
    pub const CRON_INVALID_EXPR: i32 = -33302;
    /// `-33303` — daily run / cost budget exceeded.
    pub const CRON_BUDGET_EXCEEDED: i32 = -33303;
    /// `-33304` — invalid IANA timezone in `tz`.
    pub const CRON_INVALID_TZ: i32 = -33304;
}

/// Classifies an internal error before it crosses the IPC boundary, so
/// [`to_rpc_error`] can pick the right JSON-RPC code without callers having
/// to remember the numeric ranges in [`error_codes`].
///
/// Centralising the mapping here is the architecture's anti-leak guard
/// (§9.1: "绝不把内部 panic 跨进程透传"): every place that turns an internal
/// `DaemonError` into a wire error funnels through [`to_rpc_error`], and
/// the unit test in `tests/round_trip.rs` pins the variant→code table so a
/// silent reassignment can't sneak past review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Bad envelope (JSON parse failure). Maps to `PARSE_ERROR`.
    Parse,
    /// Spec-shaped invalid request (e.g. missing `method`). Maps to
    /// `INVALID_REQUEST`.
    InvalidRequest,
    /// No such method on this daemon build. Maps to `METHOD_NOT_FOUND`.
    MethodNotFound,
    /// Typed params failed to decode. Maps to `INVALID_PARAMS`.
    InvalidParams,
    /// Generic server-side failure that isn't business-meaningful (panic
    /// caught at the boundary, IO error during framing, etc.). Maps to
    /// `INTERNAL_ERROR`.
    Internal,
    /// Reserved server-implementation range (`-32099..-32000`); use for
    /// handshake or transport-layer failures the spec leaves to us. Maps to
    /// `SERVER_ERROR_START`.
    Server,
    // ---- business ----
    NotInitialized,
    UnsupportedProtocolVersion,
    SessionNotFound,
    WriterLocked,
    NotAttached,
    ReplayOutOfRange,
    SessionBusy,
    PayloadTooLarge,
    UnknownEventTopic,
    AdapterNotInstalled,
    AdapterUnauthenticated,
    AdapterSpawnFailed,
    AdapterProtocolDrift,
    AdapterUnsupportedOption,
    StorageBusy,
    StorageConflict,
    StorageFailed,
    CronNotFound,
    CronInvalidExpr,
    CronBudgetExceeded,
    CronInvalidTz,
}

impl ErrorKind {
    /// Numeric JSON-RPC code for this kind. The mapping is pinned by a unit
    /// test in `tests/round_trip.rs`.
    pub const fn code(self) -> i32 {
        use error_codes::*;
        match self {
            ErrorKind::Parse => PARSE_ERROR,
            ErrorKind::InvalidRequest => INVALID_REQUEST,
            ErrorKind::MethodNotFound => METHOD_NOT_FOUND,
            ErrorKind::InvalidParams => INVALID_PARAMS,
            ErrorKind::Internal => INTERNAL_ERROR,
            ErrorKind::Server => SERVER_ERROR_START,
            ErrorKind::NotInitialized => NOT_INITIALIZED,
            ErrorKind::UnsupportedProtocolVersion => UNSUPPORTED_PROTOCOL_VERSION,
            ErrorKind::SessionNotFound => SESSION_NOT_FOUND,
            ErrorKind::WriterLocked => WRITER_LOCKED,
            ErrorKind::NotAttached => NOT_ATTACHED,
            ErrorKind::ReplayOutOfRange => REPLAY_OUT_OF_RANGE,
            ErrorKind::SessionBusy => SESSION_BUSY,
            ErrorKind::PayloadTooLarge => PAYLOAD_TOO_LARGE,
            ErrorKind::UnknownEventTopic => UNKNOWN_EVENT_TOPIC,
            ErrorKind::AdapterNotInstalled => ADAPTER_NOT_INSTALLED,
            ErrorKind::AdapterUnauthenticated => ADAPTER_UNAUTHENTICATED,
            ErrorKind::AdapterSpawnFailed => ADAPTER_SPAWN_FAILED,
            ErrorKind::AdapterProtocolDrift => ADAPTER_PROTOCOL_DRIFT,
            ErrorKind::AdapterUnsupportedOption => ADAPTER_UNSUPPORTED_OPTION,
            ErrorKind::StorageBusy => STORAGE_BUSY,
            ErrorKind::StorageConflict => STORAGE_CONFLICT,
            ErrorKind::StorageFailed => STORAGE_FAILED,
            ErrorKind::CronNotFound => CRON_NOT_FOUND,
            ErrorKind::CronInvalidExpr => CRON_INVALID_EXPR,
            ErrorKind::CronBudgetExceeded => CRON_BUDGET_EXCEEDED,
            ErrorKind::CronInvalidTz => CRON_INVALID_TZ,
        }
    }
}

/// Build a [`jsonrpc::RpcError`] from a kind, message, and optional
/// structured `data` payload.
///
/// This is the single sanctioned funnel from internal errors to wire errors
/// (architecture §9.1). The `data` argument is generic so adapters can attach
/// typed diagnostics (e.g. `{"docs_url":"…"}` for
/// [`ErrorKind::AdapterUnauthenticated`]); passing `()` skips the field.
///
/// Returns `Err` only when `data` cannot be serialized — never on the
/// kind→code mapping, which is infallible.
pub fn to_rpc_error<D: Serialize>(
    kind: ErrorKind,
    message: impl Into<String>,
    data: D,
) -> Result<jsonrpc::RpcError, serde_json::Error> {
    let mut err = jsonrpc::RpcError::new(kind.code(), message);
    let value = serde_json::to_value(data)?;
    // Treat unit `()` as "no data attached" so the JSON envelope stays
    // clean (`{"code":…,"message":…}` not `{"code":…,"message":…,"data":null}`).
    if !value.is_null() {
        err.data = Some(value);
    }
    Ok(err)
}
