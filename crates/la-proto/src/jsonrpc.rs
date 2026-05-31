//! Generic JSON-RPC 2.0 envelopes.
//!
//! Three message shapes are defined by the spec; we mirror them as separate
//! types instead of a tagged enum, so that producers don't accidentally build
//! invalid messages (e.g. a "response" with no id, or a "notification" with
//! an id). The cross-cutting [`Message`] enum decodes any of the three.
//!
//! ### Important invariants enforced by serde
//!
//! - `jsonrpc` is always the literal `"2.0"` — we serialize a unit `Version`
//!   field so consumers cannot forget it, and we reject other values on deserialize.
//! - A [`Response`] carries **either** `result` xor `error`; we model that
//!   with the [`ResponseOutcome`] enum and flatten it during (de)serialization.
//! - A [`Notification`] never has an `id`; the spec lets servers send
//!   "asynchronous" notifications without correlation, which is exactly what
//!   we need for `session.output`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error_codes;

/// JSON-RPC version marker. Always serializes/deserializes as `"2.0"`.
///
/// Modeled as a unit struct so the version is impossible to misspell at the
/// type level, while still being a single field in the JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Use owned String (not &str) so we deserialize correctly from both
        // a raw JSON byte stream AND a fully-owned serde_json::Value (where
        // borrowed strings aren't available).
        let s = String::deserialize(d)?;
        if s == "2.0" {
            Ok(Version)
        } else {
            Err(serde::de::Error::custom(format!(
                "expected jsonrpc = \"2.0\", got {:?}",
                s
            )))
        }
    }
}

/// Stable identifier on a request/response pair.
///
/// The spec allows string, number, or null. Requests SHOULD use string or
/// number; the [`RequestId::Null`] variant exists because the spec also
/// REQUIRES it in one specific case: when a server cannot detect the id of
/// an incoming request (parse error / invalid request envelope) it MUST
/// reply with `id: null` (JSON-RPC 2.0 §5.1). Constructors for outbound
/// requests should prefer [`RequestId::Num`] or [`RequestId::Str`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    /// A signed 64-bit integer id.
    Num(i64),
    /// An opaque string id.
    Str(String),
    /// JSON `null` — used by servers in error responses when the request id
    /// could not be determined. Only the server side should construct this.
    Null,
}

impl Serialize for RequestId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            RequestId::Num(n) => s.serialize_i64(*n),
            RequestId::Str(v) => s.serialize_str(v),
            // Critical: must serialize as JSON null (not as the literal string
            // "null" or as an absent field), so the spec-compliant error
            // response shape `{"jsonrpc":"2.0","id":null,"error":{...}}` is
            // produced byte-for-byte.
            RequestId::Null => s.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Visitor;
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RequestId;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("jsonrpc id: string, integer, or null")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<RequestId, E> {
                Ok(RequestId::Str(s.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<RequestId, E> {
                Ok(RequestId::Str(s))
            }
            fn visit_i64<E: serde::de::Error>(self, n: i64) -> Result<RequestId, E> {
                Ok(RequestId::Num(n))
            }
            fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<RequestId, E> {
                Ok(RequestId::Num(n as i64))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<RequestId, E> {
                Ok(RequestId::Null)
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<RequestId, E> {
                Ok(RequestId::Null)
            }
        }
        d.deserialize_any(V)
    }
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        RequestId::Num(n)
    }
}

impl From<String> for RequestId {
    fn from(s: String) -> Self {
        RequestId::Str(s)
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        RequestId::Str(s.to_owned())
    }
}

/// A JSON-RPC 2.0 request.
///
/// `params` is kept as `serde_json::Value` so the framing layer can route on
/// `method` before paying the cost of typed parameter parsing. Use
/// [`Request::params_as`] / [`Request::params_into`] to get a typed struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: Version,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Build a typed request, serializing `params` lazily.
    pub fn new<P: Serialize>(
        id: impl Into<RequestId>,
        method: impl Into<String>,
        params: P,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: Version,
            id: id.into(),
            method: method.into(),
            params: Some(serde_json::to_value(params)?),
        })
    }

    /// Borrow `params` as a typed value without consuming the request.
    ///
    /// Returns an error if `params` is absent and `T` does not have a default-
    /// shaped representation; for empty-param methods, prefer `Option<P>` or
    /// a struct with `#[serde(default)]`.
    pub fn params_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        match &self.params {
            Some(v) => serde_json::from_value(v.clone()),
            None => serde_json::from_value(Value::Null),
        }
    }

    /// Consume the request and produce a typed `params` value.
    pub fn params_into<T: for<'de> Deserialize<'de>>(self) -> Result<T, serde_json::Error> {
        match self.params {
            Some(v) => serde_json::from_value(v),
            None => serde_json::from_value(Value::Null),
        }
    }
}

/// Server → client async notification (no id, no response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: Version,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new<P: Serialize>(
        method: impl Into<String>,
        params: P,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: Version,
            method: method.into(),
            params: Some(serde_json::to_value(params)?),
        })
    }

    pub fn params_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        match &self.params {
            Some(v) => serde_json::from_value(v.clone()),
            None => serde_json::from_value(Value::Null),
        }
    }
}

/// Response payload — either `result` or `error`, never both.
///
/// Deserialization is strict: a payload carrying both fields, or neither,
/// is rejected (JSON-RPC 2.0 §5: "Either the result member or error member
/// MUST be included, but both members MUST NOT be included.").
#[derive(Debug, Clone)]
pub enum ResponseOutcome {
    /// Success path; field name matches JSON-RPC spec.
    Result(Value),
    /// Failure path; field name matches JSON-RPC spec.
    Error(RpcError),
}

impl Serialize for ResponseOutcome {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(1))?;
        match self {
            ResponseOutcome::Result(v) => m.serialize_entry("result", v)?,
            ResponseOutcome::Error(e) => m.serialize_entry("error", e)?,
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for ResponseOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Decode into an optional-pair temp and then enforce exclusivity by
        // hand — `#[serde(untagged)]` would silently accept both fields
        // because untagged variants are tried in order with no field-set
        // checking.
        #[derive(Deserialize)]
        struct Both {
            #[serde(default)]
            result: Option<Value>,
            #[serde(default)]
            error: Option<RpcError>,
        }
        let Both { result, error } = Both::deserialize(d)?;
        match (result, error) {
            (Some(r), None) => Ok(ResponseOutcome::Result(r)),
            (None, Some(e)) => Ok(ResponseOutcome::Error(e)),
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "JSON-RPC response must carry exactly one of `result` or `error`, not both",
            )),
            (None, None) => Err(serde::de::Error::custom(
                "JSON-RPC response must carry one of `result` or `error`",
            )),
        }
    }
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: Version,
    pub id: RequestId,
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}

impl Response {
    pub fn success<R: Serialize>(id: RequestId, result: R) -> Result<Self, serde_json::Error> {
        Ok(Self {
            jsonrpc: Version,
            id,
            outcome: ResponseOutcome::Result(serde_json::to_value(result)?),
        })
    }

    pub fn error(id: RequestId, error: RpcError) -> Self {
        Self {
            jsonrpc: Version,
            id,
            outcome: ResponseOutcome::Error(error),
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("jsonrpc error {code}: {message}")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data<D: Serialize>(mut self, data: D) -> Result<Self, serde_json::Error> {
        self.data = Some(serde_json::to_value(data)?);
        Ok(self)
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(error_codes::PARSE_ERROR, message)
    }
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(error_codes::INVALID_REQUEST, message)
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            error_codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(error_codes::INVALID_PARAMS, message)
    }
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(error_codes::INTERNAL_ERROR, message)
    }
}

/// Cross-cutting decode helper: classify any incoming frame as request,
/// response, or notification without paying the typed-params cost yet.
///
/// The classification rule (per spec): a JSON-RPC message with `method` and
/// `id` is a [`Request`]; with `method` but no `id` is a [`Notification`];
/// with `id` but no `method` is a [`Response`]. Anything else is invalid.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

impl Message {
    /// Decode a single message from raw bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, RpcError> {
        // We do a two-step decode so the error message is shaped like a
        // proper JSON-RPC error rather than serde's terse "expected ..."
        // text. Frames that don't even parse as JSON return PARSE_ERROR;
        // frames that parse but don't match any envelope return INVALID_REQUEST.
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|e| RpcError::parse_error(format!("invalid JSON: {e}")))?;
        serde_json::from_value(value)
            .map_err(|e| RpcError::invalid_request(format!("not a valid JSON-RPC message: {e}")))
    }
}
