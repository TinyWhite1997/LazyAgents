//! Round-trip tests for envelopes, typed methods, base64 payloads, and
//! the 64 KiB chunker.
//!
//! Failure modes covered:
//! - request/notification/response classification via [`Message`]
//! - the `Version` newtype refusing non-2.0 inputs
//! - base64 round-trip preserves bytes including NULs / non-UTF-8
//! - `chunk_session_output` honours the 64 KiB cap and increments `seq`
//!   monotonically across chunks
//! - empty data still emits one heartbeat chunk
//! - response outcome is exactly `result` xor `error`

use la_proto::chunking::chunk_session_output;
use la_proto::jsonrpc::{Message, Notification, Request, RequestId, Response, RpcError, Version};
use la_proto::methods::{
    Initialize, InitializeParams, InitializeResult, Method, ServerCapabilities, SessionsAttach,
    SessionsAttachParams, SessionsCreate, SessionsCreateParams, SessionsCreateResult, PtySize,
    SessionState, SessionsWrite, SessionsWriteParams, SessionsWriteResult,
};
use la_proto::notifications::{NotificationMethod, SessionOutput, SessionOutputParams};
use la_proto::{PROTOCOL_VERSION, SESSION_OUTPUT_CHUNK_BYTES};
use serde_json::json;

#[test]
fn version_rejects_non_2_0() {
    let bad = serde_json::from_str::<Version>("\"1.0\"");
    assert!(bad.is_err());
    let good = serde_json::from_str::<Version>("\"2.0\"").unwrap();
    assert_eq!(good, Version);
}

#[test]
fn version_always_serializes_as_2_0() {
    let s = serde_json::to_string(&Version).unwrap();
    assert_eq!(s, "\"2.0\"");
}

#[test]
fn message_decodes_request() {
    let bytes = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
    let m = Message::from_slice(bytes).unwrap();
    match m {
        Message::Request(r) => {
            assert_eq!(r.method, "ping");
            assert_eq!(r.id, RequestId::Num(1));
        }
        other => panic!("expected request, got {:?}", other),
    }
}

#[test]
fn message_decodes_notification() {
    let bytes = br#"{"jsonrpc":"2.0","method":"hello"}"#;
    let m = Message::from_slice(bytes).unwrap();
    assert!(matches!(m, Message::Notification(_)));
}

#[test]
fn message_decodes_response_result() {
    let bytes = br#"{"jsonrpc":"2.0","id":"abc","result":{"ok":true}}"#;
    let m = Message::from_slice(bytes).unwrap();
    match m {
        Message::Response(r) => assert_eq!(r.id, RequestId::Str("abc".into())),
        other => panic!("expected response, got {:?}", other),
    }
}

#[test]
fn message_decodes_response_error() {
    let bytes = br#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"nope"}}"#;
    let m = Message::from_slice(bytes).unwrap();
    assert!(matches!(m, Message::Response(_)));
}

#[test]
fn message_parse_error_classification() {
    let bytes = b"not even json";
    let err = Message::from_slice(bytes).unwrap_err();
    assert_eq!(err.code, la_proto::error_codes::PARSE_ERROR);

    let bytes = br#"{"some":"object"}"#;
    let err = Message::from_slice(bytes).unwrap_err();
    assert_eq!(err.code, la_proto::error_codes::INVALID_REQUEST);
}

#[test]
fn request_typed_round_trip_initialize() {
    let params = InitializeParams {
        client: "la".into(),
        client_version: "0.4.1".into(),
        protocol_versions: vec![PROTOCOL_VERSION.into()],
    };
    let req = Request::new(1i64, Initialize::NAME, &params).unwrap();
    let bytes = serde_json::to_vec(&req).unwrap();
    let decoded = Message::from_slice(&bytes).unwrap();
    let Message::Request(r) = decoded else {
        panic!("not a request");
    };
    let round: InitializeParams = r.params_into().unwrap();
    assert_eq!(round, params);
}

#[test]
fn initialize_result_round_trip() {
    let res = InitializeResult {
        server: "lad".into(),
        server_version: "0.4.1".into(),
        protocol_version: PROTOCOL_VERSION.into(),
        capabilities: ServerCapabilities {
            adapters: vec!["claude".into()],
            cron: true,
            worktree: true,
        },
    };
    let resp = Response::success(RequestId::Num(1), &res).unwrap();
    let s = serde_json::to_string(&resp).unwrap();
    // Must have a "result" field and NOT have an "error" field.
    assert!(s.contains("\"result\""));
    assert!(!s.contains("\"error\""));
    let decoded = Message::from_slice(s.as_bytes()).unwrap();
    let Message::Response(r) = decoded else {
        panic!("not a response");
    };
    let back: InitializeResult = match r.outcome {
        la_proto::jsonrpc::ResponseOutcome::Result(result) => {
            serde_json::from_value(result).unwrap()
        }
        la_proto::jsonrpc::ResponseOutcome::Error(error) => panic!("got error: {error:?}"),
    };
    assert_eq!(back, res);
}

#[test]
fn response_error_round_trip() {
    let err = RpcError::method_not_found("sessions.fly");
    let resp = Response::error(RequestId::Num(7), err.clone());
    let bytes = serde_json::to_vec(&resp).unwrap();
    let decoded = Message::from_slice(&bytes).unwrap();
    let Message::Response(r) = decoded else { panic!() };
    let back = match r.outcome {
        la_proto::jsonrpc::ResponseOutcome::Error(error) => error,
        _ => panic!("expected error outcome"),
    };
    assert_eq!(back.code, err.code);
    assert_eq!(back.message, err.message);
}

#[test]
fn sessions_create_round_trip() {
    let params = SessionsCreateParams {
        project_dir: "/tmp/p".into(),
        backend: "claude".into(),
        args: vec!["--resume".into()],
        prompt: Some("hello".into()),
        worktree: false,
    };
    let req = Request::new("rpc-2", SessionsCreate::NAME, &params).unwrap();
    let bytes = serde_json::to_vec(&req).unwrap();
    let m = Message::from_slice(&bytes).unwrap();
    let Message::Request(r) = m else { panic!() };
    assert_eq!(r.method, "sessions.create");
    let p: SessionsCreateParams = r.params_into().unwrap();
    assert_eq!(p, params);

    let result = SessionsCreateResult {
        session_id: "01J0...".into(),
        backend: "claude".into(),
        cwd: "/tmp/p".into(),
        initial_size: PtySize { rows: 24, cols: 80 },
        state: SessionState::Starting,
    };
    let resp = Response::success(RequestId::Str("rpc-2".into()), &result).unwrap();
    let back: SessionsCreateResult =
        match Message::from_slice(&serde_json::to_vec(&resp).unwrap()).unwrap() {
            Message::Response(Response {
                outcome: la_proto::jsonrpc::ResponseOutcome::Result(result),
                ..
            }) => serde_json::from_value(result).unwrap(),
            _ => panic!(),
        };
    assert_eq!(back, result);
}

#[test]
fn sessions_attach_round_trip() {
    let params = SessionsAttachParams {
        session_id: "abc".into(),
        replay_bytes: Some(4096),
        acquire_input: true,
    };
    let s = serde_json::to_string(&params).unwrap();
    let back: SessionsAttachParams = serde_json::from_str(&s).unwrap();
    assert_eq!(back, params);
    assert_eq!(SessionsAttach::NAME, "sessions.attach");
}

#[test]
fn sessions_write_base64_preserves_arbitrary_bytes() {
    // Includes NUL, high byte, newline, ESC.
    let raw: &[u8] = &[0x00, 0xff, b'\n', 0x1b, b'h', b'i'];
    let p = SessionsWriteParams::from_bytes("sid", raw);
    let s = serde_json::to_string(&p).unwrap();
    let back: SessionsWriteParams = serde_json::from_str(&s).unwrap();
    assert_eq!(back.data_bytes().unwrap(), raw);
    assert_eq!(SessionsWrite::NAME, "sessions.write");
    // Sanity: empty write result has no fields on the wire.
    let r = serde_json::to_string(&SessionsWriteResult::default()).unwrap();
    assert_eq!(r, "{}");
}

#[test]
fn session_output_notification_round_trip() {
    let n_params = SessionOutputParams::from_bytes("sid", 42, b"hello");
    let n = Notification::new(SessionOutput::NAME, &n_params).unwrap();
    let bytes = serde_json::to_vec(&n).unwrap();
    let decoded = Message::from_slice(&bytes).unwrap();
    let Message::Notification(nn) = decoded else { panic!() };
    let p: SessionOutputParams = nn.params_as().unwrap();
    assert_eq!(p, n_params);
    assert_eq!(p.data_bytes().unwrap(), b"hello");
}

#[test]
fn chunker_respects_64kib_cap_and_monotonic_seq() {
    let data = vec![b'a'; 3 * SESSION_OUTPUT_CHUNK_BYTES + 17];
    let chunks = chunk_session_output("sid", 100, &data);
    assert_eq!(chunks.len(), 4); // 64K, 64K, 64K, 17
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.seq, 100 + i as u64);
        let decoded = c.data_bytes().unwrap();
        assert!(decoded.len() <= SESSION_OUTPUT_CHUNK_BYTES);
    }
    let total: usize = chunks.iter().map(|c| c.data_bytes().unwrap().len()).sum();
    assert_eq!(total, data.len());
}

#[test]
fn chunker_emits_single_heartbeat_for_empty_data() {
    let chunks = chunk_session_output("sid", 0, &[]);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].seq, 0);
    assert_eq!(chunks[0].data_bytes().unwrap(), Vec::<u8>::new());
}

#[test]
fn method_not_found_error_helper() {
    let e = RpcError::method_not_found("xyz");
    let v: serde_json::Value = serde_json::to_value(&e).unwrap();
    assert_eq!(v, json!({"code": -32601, "message": "method not found: xyz"}));
}

#[test]
fn request_id_null_serializes_as_json_null() {
    // Spec §5.1: a parse-error / invalid-request response MUST carry id: null.
    let resp = Response::error(RequestId::Null, RpcError::parse_error("garbage"));
    let v: serde_json::Value = serde_json::to_value(&resp).unwrap();
    assert!(v.get("id").is_some(), "id field must be present");
    assert!(v["id"].is_null(), "id field must be JSON null, got {:?}", v["id"]);
    // Round-trip.
    let bytes = serde_json::to_vec(&resp).unwrap();
    let m = Message::from_slice(&bytes).unwrap();
    let Message::Response(r) = m else { panic!("not a response") };
    assert_eq!(r.id, RequestId::Null);
}

#[test]
fn response_outcome_rejects_both_result_and_error() {
    // Spec §5: "Either the result member or error member MUST be included,
    // but both members MUST NOT be included."
    let bytes = br#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":-1,"message":"x"}}"#;
    let err = Message::from_slice(bytes).unwrap_err();
    assert_eq!(err.code, la_proto::error_codes::INVALID_REQUEST);
}

#[test]
fn response_outcome_rejects_neither_result_nor_error() {
    let bytes = br#"{"jsonrpc":"2.0","id":1}"#;
    let err = Message::from_slice(bytes).unwrap_err();
    assert_eq!(err.code, la_proto::error_codes::INVALID_REQUEST);
}

#[test]
fn sessions_write_try_from_bytes_refuses_oversize() {
    let too_big = vec![0u8; SessionsWriteParams::MAX_RAW_BYTES + 1];
    let err = SessionsWriteParams::try_from_bytes("sid", &too_big).unwrap_err();
    assert_eq!(err.limit, SessionsWriteParams::MAX_RAW_BYTES);
    // And the wire-encoded size stays under the message cap for the maximum
    // allowed input.
    let max_ok = vec![0u8; SessionsWriteParams::MAX_RAW_BYTES];
    let p = SessionsWriteParams::try_from_bytes("sid", &max_ok).unwrap();
    let req = Request::new(1i64, SessionsWrite::NAME, &p).unwrap();
    let wire = serde_json::to_vec(&Message::Request(req)).unwrap();
    assert!(
        wire.len() <= la_proto::MAX_MESSAGE_BYTES,
        "encoded request {} > {}",
        wire.len(),
        la_proto::MAX_MESSAGE_BYTES
    );
}
