//! Version-negotiating handshake.
//!
//! Both sides run this immediately after the transport connects. The client
//! sends an `initialize` request listing its supported protocol majors; the
//! server picks the highest one it also supports and replies. If the
//! intersection is empty, the server SHOULD reply with an
//! `UNSUPPORTED_PROTOCOL_VERSION` error and then the connection is closed.
//! (Closing is the caller's job; the helpers here only do the protocol-level
//! exchange.)
//!
//! Both sides accept a list of "what I support" so a forward-compatible
//! daemon can offer `["1"]` today and `["2", "1"]` tomorrow without
//! breaking older clients.

use std::collections::HashSet;

use la_proto::error_codes;
use la_proto::jsonrpc::{Message, Request, Response, ResponseOutcome, RpcError};
use la_proto::methods::{
    Initialize, InitializeParams, InitializeResult, Method, ServerCapabilities,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::connection::Connection;
use crate::IpcError;

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// Server sent something that wasn't a response to our initialize, or
    /// closed the connection mid-handshake.
    #[error("unexpected message during handshake: {0}")]
    UnexpectedMessage(String),

    /// Server replied with a JSON-RPC error.
    #[error("server rejected handshake (code {code}): {message}")]
    ServerRejected { code: i32, message: String },

    /// No overlap between the client's and server's supported protocol
    /// majors.
    #[error("no common protocol version; client={client:?}, server={server:?}")]
    NoCommonVersion {
        client: Vec<String>,
        server: Vec<String>,
    },

    /// Server returned a protocol version we didn't offer. Treat as
    /// protocol drift; close the connection.
    #[error("server picked unsupported version {0:?}")]
    BadServerPick(String),
}

/// Information returned from a successful server handshake.
///
/// Mirrors [`InitializeResult`] one-for-one; aliased so call sites can
/// avoid pulling `la_proto` in just for the type name.
pub type ServerInfo = InitializeResult;

/// Run the client side of the handshake.
///
/// Sends `initialize` once, awaits one response, returns the negotiated
/// [`ServerInfo`]. The returned [`Connection`] is ready for general use.
pub async fn client_handshake<S>(
    conn: &mut Connection<S>,
    client_name: &str,
    client_version: &str,
    supported_protocol_versions: &[&str],
) -> Result<ServerInfo, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let params = InitializeParams {
        client: client_name.to_owned(),
        client_version: client_version.to_owned(),
        protocol_versions: supported_protocol_versions.iter().map(|s| s.to_string()).collect(),
    };
    let req = Request::new(1i64, Initialize::NAME, &params)
        .map_err(|e| HandshakeError::UnexpectedMessage(format!("encode initialize: {e}")))?;

    conn.send(&Message::Request(req))
        .await
        .map_err(io_to_handshake)?;

    let msg = conn
        .recv()
        .await
        .map_err(io_to_handshake)?
        .ok_or_else(|| HandshakeError::UnexpectedMessage("EOF before initialize response".into()))?;

    let resp = match msg {
        Message::Response(r) => r,
        Message::Request(r) => {
            return Err(HandshakeError::UnexpectedMessage(format!(
                "expected response, got request {:?}",
                r.method
            )))
        }
        Message::Notification(n) => {
            return Err(HandshakeError::UnexpectedMessage(format!(
                "expected response, got notification {:?}",
                n.method
            )))
        }
    };

    let result = match resp.outcome {
        ResponseOutcome::Result(result) => result,
        ResponseOutcome::Error(error) => {
            return Err(HandshakeError::ServerRejected {
                code: error.code,
                message: error.message,
            })
        }
    };

    let info: ServerInfo = serde_json::from_value(result)
        .map_err(|e| HandshakeError::UnexpectedMessage(format!("decode result: {e}")))?;

    if !supported_protocol_versions
        .iter()
        .any(|v| *v == info.protocol_version)
    {
        return Err(HandshakeError::BadServerPick(info.protocol_version));
    }
    Ok(info)
}

/// Run the server side of the handshake.
///
/// Awaits one `initialize` request, picks the first version the server lists
/// that the client also offers (so the caller MUST list versions in its
/// preference order — most preferred first), and replies. On no-overlap,
/// sends an error response and returns [`HandshakeError::NoCommonVersion`]
/// without closing the underlying connection — the caller drops it.
pub async fn server_handshake<S>(
    conn: &mut Connection<S>,
    server_name: &str,
    server_version: &str,
    supported_protocol_versions: &[&str],
    capabilities: ServerCapabilities,
) -> Result<InitializeParams, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let first = conn
        .recv()
        .await
        .map_err(io_to_handshake)?
        .ok_or_else(|| HandshakeError::UnexpectedMessage("EOF before initialize".into()))?;

    let req = match first {
        Message::Request(r) if r.method == Initialize::NAME => r,
        Message::Request(r) => {
            // Not `initialize` first — reply with NOT_INITIALIZED so the
            // client sees a structured rejection rather than a hang.
            let id = r.id.clone();
            let err = RpcError::new(
                error_codes::NOT_INITIALIZED,
                "first request must be \"initialize\"",
            );
            let _ = conn
                .send(&Message::Response(Response::error(id, err)))
                .await;
            return Err(HandshakeError::UnexpectedMessage(format!(
                "first request was {:?}, expected initialize",
                r.method
            )));
        }
        other => {
            return Err(HandshakeError::UnexpectedMessage(format!(
                "expected initialize request, got {:?}",
                other
            )))
        }
    };

    let id_for_reply = req.id.clone();
    let params: InitializeParams = req
        .params_into()
        .map_err(|e| HandshakeError::UnexpectedMessage(format!("decode initialize: {e}")))?;

    let client_set: HashSet<&str> = params
        .protocol_versions
        .iter()
        .map(String::as_str)
        .collect();
    // Caller MUST list `supported_protocol_versions` in preference order
    // (highest/most-preferred first); the first server-listed version that
    // the client also supports wins. We intentionally do not do numeric
    // comparison — protocol-version strings are opaque tokens.
    let picked = supported_protocol_versions
        .iter()
        .find(|v| client_set.contains(*v))
        .copied();

    let Some(picked) = picked else {
        // On the wire: do NOT leak the server's supported-versions list.
        // The client already knows its own list, and revealing the server's
        // doesn't help them retry. We only carry the client list in the
        // local `HandshakeError::NoCommonVersion` for the daemon's logs.
        let err = RpcError::new(
            error_codes::UNSUPPORTED_PROTOCOL_VERSION,
            "no common protocol version",
        );
        let _ = conn
            .send(&Message::Response(Response::error(id_for_reply, err)))
            .await;
        return Err(HandshakeError::NoCommonVersion {
            client: params.protocol_versions.clone(),
            server: supported_protocol_versions
                .iter()
                .map(|s| s.to_string())
                .collect(),
        });
    };

    let result = InitializeResult {
        server: server_name.to_owned(),
        server_version: server_version.to_owned(),
        protocol_version: picked.to_owned(),
        capabilities,
    };
    let resp = Response::success(id_for_reply, &result)
        .map_err(|e| HandshakeError::UnexpectedMessage(format!("encode result: {e}")))?;
    conn.send(&Message::Response(resp))
        .await
        .map_err(io_to_handshake)?;
    Ok(params)
}

fn io_to_handshake(e: IpcError) -> HandshakeError {
    HandshakeError::UnexpectedMessage(format!("io: {e}"))
}
