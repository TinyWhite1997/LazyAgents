//! Length-prefixed JSON-RPC transport primitives for LazyAgents.
//!
//! The crate intentionally keeps the OS endpoint out of scope for M0: callers
//! can place [`FramedJson`] on top of UDS, named pipes, TCP, or in-memory
//! streams. The wire contract is a big-endian `u32` byte length followed by a
//! single UTF-8 JSON document.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame too large: {actual} bytes exceeds {max} bytes")]
    FrameTooLarge { actual: usize, max: usize },
    #[error("connection closed")]
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl RpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcErrorObject>,
}

impl RpcResponse {
    pub fn result(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(RpcErrorObject {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
}

pub struct FramedJson<S> {
    stream: S,
    max_frame_len: usize,
}

impl<S> FramedJson<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            max_frame_len: MAX_FRAME_LEN,
        }
    }

    pub fn with_max_frame_len(stream: S, max_frame_len: usize) -> Self {
        Self {
            stream,
            max_frame_len,
        }
    }

    pub async fn read_json<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
        let len = match self.stream.read_u32().await {
            Ok(len) => len as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(IpcError::Closed)
            }
            Err(e) => return Err(IpcError::Io(e)),
        };
        if len > self.max_frame_len {
            return Err(IpcError::FrameTooLarge {
                actual: len,
                max: self.max_frame_len,
            });
        }

        let mut buf = vec![0; len];
        self.stream.read_exact(&mut buf).await?;
        Ok(serde_json::from_slice(&buf)?)
    }

    pub async fn write_json<T: Serialize>(&mut self, value: &T) -> Result<(), IpcError> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > self.max_frame_len {
            return Err(IpcError::FrameTooLarge {
                actual: bytes.len(),
                max: self.max_frame_len,
            });
        }

        self.stream.write_u32(bytes.len() as u32).await?;
        self.stream.write_all(&bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn round_trips_json_rpc_request() {
        let (client, server) = tokio::io::duplex(1024);
        let mut client = FramedJson::new(client);
        let mut server = FramedJson::new(server);

        let request = RpcRequest::new(7, "sessions.attach", json!({ "session_id": "s1" }));
        client.write_json(&request).await.expect("write request");

        let decoded: RpcRequest = server.read_json().await.expect("read request");
        assert_eq!(decoded, request);
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let (client, server) = tokio::io::duplex(1024);
        let mut client = FramedJson::new(client);
        let mut server = FramedJson::with_max_frame_len(server, 8);

        client
            .write_json(&serde_json::json!({ "too": "large" }))
            .await
            .expect("write frame");
        let err = server
            .read_json::<serde_json::Value>()
            .await
            .expect_err("oversized frame");
        assert!(matches!(err, IpcError::FrameTooLarge { .. }));
    }
}
