//! Wire contracts for the LazyAgents daemon ↔ client protocol.
//!
//! This crate intentionally has **no** transport dependencies: it only defines
//! the JSON-RPC 2.0 envelope and the M0.2 minimum method set (`initialize`,
//! `sessions.create`, `sessions.attach`, `sessions.write`) plus the
//! `session.output` server notification. Transport (UDS / Named Pipe,
//! length-prefix framing) lives in `la-ipc`.
//!
//! ## Layout
//!
//! - [`jsonrpc`] — generic JSON-RPC 2.0 envelopes ([`Request`], [`Response`],
//!   [`Notification`], [`RpcError`]). Method names and params are erased to
//!   `String` / `serde_json::Value` here so a single decode call can dispatch
//!   to typed payloads in [`methods`] / [`notifications`].
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

/// The single supported protocol major version for M0.
///
/// Negotiated via [`methods::Initialize`]. Newer minors add fields without
/// changing this value; a breaking change bumps it and the daemon must keep
/// supporting `N-1` for one minor cycle.
pub const PROTOCOL_VERSION: &str = "1";

/// JSON-RPC error code namespace ranges (architecture §9.1).
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

    /// Server-implementation error range start (`-32000`).
    pub const SERVER_ERROR_START: i32 = -32099;
    /// Server-implementation error range end (`-32000`).
    pub const SERVER_ERROR_END: i32 = -32000;

    /// LazyAgents business errors start here (architecture §9.1: `-33000..`).
    pub const BUSINESS_ERROR_START: i32 = -33000;

    /// `-33001` — `initialize` not yet called.
    pub const NOT_INITIALIZED: i32 = -33001;
    /// `-33002` — protocol version mismatch in `initialize`.
    pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -33002;
    /// `-33003` — session id does not exist.
    pub const SESSION_NOT_FOUND: i32 = -33003;
}
