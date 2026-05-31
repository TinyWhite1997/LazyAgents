//! Typed payloads for server-pushed notifications.
//!
//! M0.2 only ships `session.output`; later milestones add `session.state`,
//! `cron.fired`, and `daemon.health` (architecture §3 核心方法集).

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// All notification methods defined by `la-proto` at M0.2.
pub const NOTIFICATION_NAMES: &[&str] = &[SessionOutput::NAME];

/// Trait mirroring [`crate::methods::Method`] but for one-way notifications
/// (no `Result` type).
///
/// Named [`NotificationMethod`] (not `Notification`) to avoid shadowing the
/// [`crate::jsonrpc::Notification`] envelope struct at call sites.
pub trait NotificationMethod {
    const NAME: &'static str;
    type Params: Serialize + for<'de> Deserialize<'de> + JsonSchema;
}

/// PTY output increment for an attached session.
///
/// `seq` is monotonically increasing per session. Chunks larger than
/// [`crate::SESSION_OUTPUT_CHUNK_BYTES`] must be split before being sent;
/// see [`crate::chunking::chunk_session_output`].
pub enum SessionOutput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(rename = "SessionOutputParams")]
pub struct SessionOutputParams {
    pub session_id: String,
    /// Monotonically increasing sequence number, per session. Clients use
    /// gaps to detect dropped frames (architecture §3 关键不变量: 背压).
    pub seq: u64,
    /// Base64-encoded PTY bytes (≤ 64 KiB per notification).
    pub data_base64: String,
}

impl SessionOutputParams {
    /// Construct a single notification payload from raw bytes.
    ///
    /// Per the architecture, a single `session.output` carries at most
    /// [`crate::SESSION_OUTPUT_CHUNK_BYTES`] (64 KiB) decoded bytes. This
    /// constructor does NOT enforce that cap (callers may briefly hold
    /// larger buffers before chunking), but the chunker
    /// [`crate::chunking::chunk_session_output`] is the canonical way to
    /// produce wire-safe values.
    pub fn from_bytes(session_id: impl Into<String>, seq: u64, data: &[u8]) -> Self {
        Self {
            session_id: session_id.into(),
            seq,
            data_base64: B64.encode(data),
        }
    }

    pub fn data_bytes(&self) -> Result<Vec<u8>, base64::DecodeError> {
        B64.decode(self.data_base64.as_bytes())
    }
}

impl NotificationMethod for SessionOutput {
    const NAME: &'static str = "session.output";
    type Params = SessionOutputParams;
}
