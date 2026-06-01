//! Length-prefix framed JSON-RPC transport for LazyAgents.
//!
//! This crate is the wire transport between the daemon (`lad`) and the
//! client (`la`). It owns:
//!
//! - The 4-byte big-endian length prefix codec ([`codec::FrameCodec`]) with a
//!   hard cap of [`MAX_MESSAGE_BYTES`] (4 MiB).
//! - Cross-platform listener / connector for UDS (Unix) and Named Pipe
//!   (Windows) under [`transport`].
//! - The version-negotiating handshake ([`handshake`]) that both sides run
//!   immediately after a connection is established.
//! - A high-level [`Connection`] handle: an async sink for outbound
//!   [`la_proto::jsonrpc::Message`] values and an async stream for inbound
//!   ones, so callers never touch raw bytes.
//!
//! No business logic lives here; method dispatch is the daemon's job and
//! lives outside this crate.

pub mod codec;
pub mod connection;
pub mod handshake;
pub mod hub;
pub mod transport;

pub use connection::{Connection, RecvHalf, SendHalf};
pub use handshake::{client_handshake, server_handshake, HandshakeError, ServerInfo};
pub use hub::{HubConfig, HubEvent, OutputHub, SubId, Subscription};

/// Maximum size of a single framed message (decoded JSON bytes), in bytes.
///
/// Matches [`la_proto::MAX_MESSAGE_BYTES`]; defined again here to keep the
/// transport layer self-contained when read in isolation.
pub const MAX_MESSAGE_BYTES: usize = la_proto::MAX_MESSAGE_BYTES;

/// Length-prefix size in bytes.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Errors produced by the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame exceeds {limit}-byte limit (got {actual})")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("encoded JSON exceeds {limit}-byte limit (got {actual})")]
    EncodeTooLarge { actual: usize, limit: usize },
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    /// JSON-RPC envelope decode failure, preserving the spec-mandated error
    /// code so the daemon can build a spec-compliant error response.
    #[error("rpc decode: {0}")]
    RpcDecode(#[from] la_proto::jsonrpc::RpcError),
    #[error("connection closed")]
    Closed,
    #[error("handshake: {0}")]
    Handshake(#[from] HandshakeError),
}
