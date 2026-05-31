//! High-level connection wrapper.
//!
//! [`Connection`] turns any `AsyncRead + AsyncWrite + Unpin` stream into a
//! pair of sinks/streams over [`la_proto::jsonrpc::Message`]. It is the API
//! both `lad` and `la` will use; raw bytes don't escape this crate.
//!
//! Split halves are provided so the daemon's read loop can sit on
//! [`RecvHalf`] while spawning per-attached-session writers that push
//! [`SendHalf`] notifications — without holding a single mutex across both.

use bytes::Bytes;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use la_proto::jsonrpc::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_util::codec::Framed;

use crate::codec::FrameCodec;
use crate::{IpcError, MAX_MESSAGE_BYTES};

/// Combined send/recv over a stream.
///
/// Cheap to construct; under the hood it is a single [`Framed`].
pub struct Connection<S> {
    framed: Framed<S, FrameCodec>,
}

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S) -> Self {
        Self {
            framed: Framed::new(stream, FrameCodec::new()),
        }
    }

    /// Send a typed message.
    ///
    /// Encoding is performed on the caller's task. Encoded messages that
    /// exceed [`MAX_MESSAGE_BYTES`] return [`IpcError::EncodeTooLarge`] —
    /// this prevents a buggy producer (e.g. a chunker handed PTY output
    /// larger than `SESSION_OUTPUT_CHUNK_BYTES * many`) from being able
    /// to put the daemon over budget.
    pub async fn send(&mut self, msg: &Message) -> Result<(), IpcError> {
        let bytes = serde_json::to_vec(msg)?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(IpcError::EncodeTooLarge {
                actual: bytes.len(),
                limit: MAX_MESSAGE_BYTES,
            });
        }
        self.framed.send(Bytes::from(bytes)).await?;
        Ok(())
    }

    /// Receive a typed message. Returns `Ok(None)` on clean EOF.
    pub async fn recv(&mut self) -> Result<Option<Message>, IpcError> {
        match self.framed.next().await {
            None => Ok(None),
            Some(frame) => Ok(Some(decode_frame(&frame?)?)),
        }
    }

    /// Split into independent send/recv halves. The send half is wrapped in
    /// a `Mutex` to keep frame boundaries intact when multiple producers
    /// (e.g. RPC responder + push-notification task) share it.
    pub fn split(self) -> (SendHalf<S>, RecvHalf<S>) {
        use futures_util::stream::StreamExt as _;
        let (sink, stream) = self.framed.split();
        (
            SendHalf {
                sink: Mutex::new(sink),
            },
            RecvHalf { stream },
        )
    }
}

/// Write half of a split [`Connection`].
pub struct SendHalf<S> {
    sink: Mutex<futures_util::stream::SplitSink<Framed<S, FrameCodec>, Bytes>>,
}

impl<S> SendHalf<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Send a single message. Multiple callers can hold this concurrently —
    /// each message goes out atomically (no interleaving inside a frame).
    pub async fn send(&self, msg: &Message) -> Result<(), IpcError> {
        let bytes = serde_json::to_vec(msg)?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(IpcError::EncodeTooLarge {
                actual: bytes.len(),
                limit: MAX_MESSAGE_BYTES,
            });
        }
        let mut guard = self.sink.lock().await;
        guard.send(Bytes::from(bytes)).await?;
        Ok(())
    }

    /// Send pre-serialized bytes (e.g. when forwarding a frame opaquely).
    pub async fn send_bytes(&self, bytes: Bytes) -> Result<(), IpcError> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(IpcError::EncodeTooLarge {
                actual: bytes.len(),
                limit: MAX_MESSAGE_BYTES,
            });
        }
        let mut guard = self.sink.lock().await;
        guard.send(bytes).await?;
        Ok(())
    }
}

/// Read half of a split [`Connection`].
pub struct RecvHalf<S> {
    stream: futures_util::stream::SplitStream<Framed<S, FrameCodec>>,
}

impl<S> RecvHalf<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Pull the next message, or `Ok(None)` on EOF.
    pub async fn recv(&mut self) -> Result<Option<Message>, IpcError> {
        match StreamExt::next(&mut self.stream).await {
            None => Ok(None),
            Some(frame) => Ok(Some(decode_frame(&frame?)?)),
        }
    }
}

/// Shared frame → typed-message decoder used by both [`Connection::recv`] and
/// [`RecvHalf::recv`]. Centralising the call site means the `RpcError`'s
/// structured `code` is preserved end-to-end, so a daemon dispatcher can
/// reply with a spec-compliant JSON-RPC error response carrying the same
/// code (PARSE_ERROR vs INVALID_REQUEST etc.) without string-grepping.
fn decode_frame(frame: &[u8]) -> Result<Message, IpcError> {
    Message::from_slice(frame).map_err(IpcError::RpcDecode)
}
