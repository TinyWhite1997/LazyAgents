//! Round-trip tests for envelopes, typed methods, base64 payloads, the
//! 64 KiB chunker, error-code mapping, and the schema golden files.
//!
//! Failure modes covered:
//! - request/notification/response classification via [`Message`]
//! - the `Version` newtype refusing non-2.0 inputs
//! - base64 round-trip preserves bytes including NULs / non-UTF-8
//! - `chunk_session_output` honours the 64 KiB cap and increments `seq`
//!   monotonically across chunks
//! - empty data still emits one heartbeat chunk
//! - response outcome is exactly `result` xor `error`
//! - every M1 method/notification serializes + decodes through `Message`
//! - `to_rpc_error` produces the right code per [`ErrorKind`] and respects
//!   the unit-data short-circuit
//! - new optional fields tolerate older-shaped payloads (no
//!   accidental wire breaks)
//! - the on-disk `docs/schema/` files match what `gen_schema` would emit
//!   right now (golden test for the "schema follows code" invariant)

use la_proto::chunking::chunk_session_output;
use la_proto::jsonrpc::{Message, Notification, Request, RequestId, Response, RpcError, Version};
use la_proto::methods::{
    EventTopic, EventsSubscribe, EventsSubscribeParams, EventsSubscribeResult, ImportedSession,
    Initialize, InitializeParams, InitializeResult, Method, PtySize, ServerCapabilities,
    SessionSignal, SessionState, SessionSummary, SessionsArchive, SessionsArchiveParams,
    SessionsAttach, SessionsAttachParams, SessionsAttachResult, SessionsCreate,
    SessionsCreateParams, SessionsCreateResult, SessionsDelete, SessionsDeleteParams,
    SessionsDetach, SessionsDetachParams, SessionsImport, SessionsImportParams,
    SessionsImportResult, SessionsList, SessionsListParams, SessionsListResult, SessionsReplay,
    SessionsReplayParams, SessionsReplayResult, SessionsResize, SessionsResizeParams,
    SessionsSignal, SessionsSignalParams, SessionsWrite, SessionsWriteParams, SessionsWriteResult,
    Shutdown, ShutdownParams, ShutdownResult,
};
use la_proto::notifications::{
    CronFired, CronFiredParams, DaemonHealth, DaemonHealthParams, NotificationMethod, SessionGap,
    SessionGapParams, SessionOutput, SessionOutputParams, SessionStateNotice, SessionStateParams,
};
use la_proto::{
    error_codes, to_rpc_error, ErrorKind, PROTOCOL_VERSION, SESSION_OUTPUT_CHUNK_BYTES,
};
use schemars::schema_for;
use serde_json::{json, Value};

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
            events: true,
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
    let Message::Response(r) = decoded else {
        panic!()
    };
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
        resume_from_seq: Some(42),
        replay_bytes: Some(4096),
        acquire_input: true,
    };
    let s = serde_json::to_string(&params).unwrap();
    let back: SessionsAttachParams = serde_json::from_str(&s).unwrap();
    assert_eq!(back, params);
    assert_eq!(SessionsAttach::NAME, "sessions.attach");
}

/// `resume_from_seq` is serde-optional (skip_serializing_if). A first-attach
/// request omits it entirely, and an old client that never knew about it
/// still decodes (back-compat for any in-the-wild serializers).
#[test]
fn sessions_attach_resume_from_seq_is_optional_on_wire() {
    // Fresh attach: no resume token, no replay window.
    let fresh = SessionsAttachParams {
        session_id: "abc".into(),
        resume_from_seq: None,
        replay_bytes: None,
        acquire_input: false,
    };
    let s = serde_json::to_string(&fresh).unwrap();
    assert!(
        !s.contains("resume_from_seq"),
        "fresh attach must not put resume_from_seq on the wire ({s})"
    );
    assert!(
        !s.contains("replay_bytes"),
        "fresh attach must not put replay_bytes on the wire ({s})"
    );

    // Legacy client that only sends {session_id, acquire_input} still parses.
    let legacy_json = r#"{"session_id":"abc","acquire_input":false}"#;
    let back: SessionsAttachParams = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(back.resume_from_seq, None);
    assert_eq!(back.replay_bytes, None);

    // Reconnect: only resume_from_seq is set.
    let resume = SessionsAttachParams {
        session_id: "abc".into(),
        resume_from_seq: Some(99),
        replay_bytes: None,
        acquire_input: false,
    };
    let v: serde_json::Value = serde_json::to_value(&resume).unwrap();
    assert_eq!(v["resume_from_seq"], json!(99));
    assert!(v.get("replay_bytes").is_none());
}

#[test]
fn sessions_attach_result_sub_token_is_optional_on_wire() {
    // Default-shaped result: no sub_token, no wire bytes for it.
    let no_token = SessionsAttachResult {
        session_id: "abc".into(),
        snapshot_seq: 7,
        input_acquired: true,
        sub_token: None,
    };
    let s = serde_json::to_string(&no_token).unwrap();
    assert!(
        !s.contains("sub_token"),
        "absent sub_token must not appear on the wire ({s})"
    );

    // Legacy result without sub_token still parses (forward-compat for the
    // M1.x daemon, which won't emit it yet).
    let legacy_json = r#"{"session_id":"abc","snapshot_seq":7,"input_acquired":true}"#;
    let back: SessionsAttachResult = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(back, no_token);

    // Round-trip with token populated (future M1.7+ daemon).
    let with_token = SessionsAttachResult {
        session_id: "abc".into(),
        snapshot_seq: 7,
        input_acquired: true,
        sub_token: Some("opaque-token-bytes".into()),
    };
    let s = serde_json::to_string(&with_token).unwrap();
    let back: SessionsAttachResult = serde_json::from_str(&s).unwrap();
    assert_eq!(back, with_token);
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
    let Message::Notification(nn) = decoded else {
        panic!()
    };
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
    assert_eq!(
        v,
        json!({"code": -32601, "message": "method not found: xyz"})
    );
}

#[test]
fn request_id_null_serializes_as_json_null() {
    // Spec §5.1: a parse-error / invalid-request response MUST carry id: null.
    let resp = Response::error(RequestId::Null, RpcError::parse_error("garbage"));
    let v: serde_json::Value = serde_json::to_value(&resp).unwrap();
    assert!(v.get("id").is_some(), "id field must be present");
    assert!(
        v["id"].is_null(),
        "id field must be JSON null, got {:?}",
        v["id"]
    );
    // Round-trip.
    let bytes = serde_json::to_vec(&resp).unwrap();
    let m = Message::from_slice(&bytes).unwrap();
    let Message::Response(r) = m else {
        panic!("not a response")
    };
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

// ----- M1.1 method/notification round-trip coverage -----

/// Round-trip every "no-params" method through a `Request` envelope so the
/// dispatcher can decode them uniformly. Shutdown/list/detach/archive etc.
/// share a shape; bundling avoids 8 nearly identical test fns.
#[test]
fn m1_envelope_round_trip_for_every_method() {
    fn roundtrip<M: Method>(params: &M::Params, result: &M::Result)
    where
        M::Params: PartialEq + std::fmt::Debug,
        M::Result: PartialEq + std::fmt::Debug,
    {
        let req = Request::new(1i64, M::NAME, params).unwrap();
        let bytes = serde_json::to_vec(&req).unwrap();
        let Message::Request(r) = Message::from_slice(&bytes).unwrap() else {
            panic!("{} not a request", M::NAME)
        };
        assert_eq!(r.method, M::NAME);
        let back: M::Params = r.params_into().unwrap();
        assert_eq!(&back, params, "{} params drift", M::NAME);

        let resp = Response::success(RequestId::Num(1), result).unwrap();
        let bytes = serde_json::to_vec(&resp).unwrap();
        let Message::Response(r) = Message::from_slice(&bytes).unwrap() else {
            panic!("{} not a response", M::NAME)
        };
        let back: M::Result = match r.outcome {
            la_proto::jsonrpc::ResponseOutcome::Result(v) => serde_json::from_value(v).unwrap(),
            _ => panic!("{} expected ok outcome", M::NAME),
        };
        assert_eq!(&back, result, "{} result drift", M::NAME);
    }

    roundtrip::<Shutdown>(&ShutdownParams::default(), &ShutdownResult::default());
    roundtrip::<SessionsList>(
        &SessionsListParams {
            project: Some("proj".into()),
            backend: Some("claude".into()),
            include_archived: true,
        },
        &SessionsListResult {
            sessions: vec![SessionSummary {
                session_id: "s1".into(),
                project_id: "p1".into(),
                backend: "claude".into(),
                title: Some("t".into()),
                state: SessionState::Running,
                origin: "user".into(),
                created_at: "2026-06-01T00:00:00Z".into(),
                updated_at: "2026-06-01T00:00:05Z".into(),
                worktree_path: Some("/wt".into()),
            }],
        },
    );
    roundtrip::<SessionsDetach>(
        &SessionsDetachParams {
            session_id: "s1".into(),
        },
        &Default::default(),
    );
    roundtrip::<SessionsResize>(
        &SessionsResizeParams {
            session_id: "s1".into(),
            cols: 120,
            rows: 40,
        },
        &Default::default(),
    );
    roundtrip::<SessionsSignal>(
        &SessionsSignalParams {
            session_id: "s1".into(),
            signal: SessionSignal::Int,
        },
        &Default::default(),
    );
    roundtrip::<SessionsArchive>(
        &SessionsArchiveParams {
            session_id: "s1".into(),
        },
        &Default::default(),
    );
    roundtrip::<SessionsDelete>(
        &SessionsDeleteParams {
            session_id: "s1".into(),
        },
        &Default::default(),
    );
    roundtrip::<SessionsImport>(
        &SessionsImportParams {
            backend: "codex".into(),
            source_path: None,
        },
        &SessionsImportResult {
            imported: vec![ImportedSession {
                session_id: "s9".into(),
                external_id: "ext-1".into(),
                backend: "codex".into(),
                project_hint: Some("/work".into()),
                created_at: "2026-05-30T08:00:00Z".into(),
                title_hint: None,
            }],
        },
    );
    roundtrip::<SessionsReplay>(
        &SessionsReplayParams {
            session_id: "s1".into(),
            from_seq: 17,
            max_bytes: Some(1024 * 1024),
        },
        &SessionsReplayResult {
            last_seq: 42,
            bytes_queued: 9000,
        },
    );
    roundtrip::<EventsSubscribe>(
        &EventsSubscribeParams {
            topics: vec![EventTopic::SessionState, EventTopic::DaemonHealth],
        },
        &EventsSubscribeResult {
            topics: vec![EventTopic::SessionState],
        },
    );
}

#[test]
fn signal_enum_serializes_uppercase_strings() {
    // The wire vocabulary is uppercase ("INT" / "TERM" / "KILL") per
    // architecture §6.3, NOT a Rust-style lowercase tag.
    assert_eq!(
        serde_json::to_value(SessionSignal::Int).unwrap(),
        Value::String("INT".into())
    );
    assert_eq!(
        serde_json::to_value(SessionSignal::Term).unwrap(),
        Value::String("TERM".into())
    );
    // Unknown signal name must be rejected at decode time so the
    // dispatcher never has to figure out what to do with it.
    let bad: Result<SessionSignal, _> = serde_json::from_value(Value::String("HUP".into()));
    assert!(bad.is_err(), "expected unknown-signal rejection");
}

#[test]
fn event_topic_uses_snake_case_on_the_wire() {
    // Snake case matches the JSON tradition the rest of the schema follows.
    assert_eq!(
        serde_json::to_value(EventTopic::SessionOutput).unwrap(),
        Value::String("session_output".into())
    );
    let bad: Result<EventTopic, _> = serde_json::from_value(Value::String("bogus".into()));
    assert!(bad.is_err(), "expected unknown-topic rejection");
}

#[test]
fn server_capabilities_tolerates_old_payload_missing_events_field() {
    // M0.2 clients/daemons emit `capabilities` without `events`. Decoding
    // such a payload must still succeed (events defaults to false), or we
    // silently broke wire compat across the M0→M1 boundary.
    let json = json!({
        "adapters": ["claude"],
        "cron": false,
        "worktree": false
    });
    let caps: ServerCapabilities = serde_json::from_value(json).unwrap();
    assert!(!caps.events, "missing field must default, not error");
    assert_eq!(caps.adapters, vec!["claude".to_string()]);
}

#[test]
fn sessions_list_params_default_is_no_filter() {
    // The TUI's default list call is `{}` (no project filter, no backend
    // filter, include_archived=false). It must decode without errors.
    let p: SessionsListParams = serde_json::from_str("{}").unwrap();
    assert_eq!(p, SessionsListParams::default());
}

#[test]
fn session_state_notification_round_trip_with_exit_code() {
    let p = SessionStateParams {
        session_id: "s1".into(),
        state: SessionState::Exited,
        exit_code: Some(0),
        reason: None,
    };
    let n = Notification::new(SessionStateNotice::NAME, &p).unwrap();
    let bytes = serde_json::to_vec(&n).unwrap();
    let Message::Notification(nn) = Message::from_slice(&bytes).unwrap() else {
        panic!()
    };
    assert_eq!(nn.method, "session.state");
    let back: SessionStateParams = nn.params_as().unwrap();
    assert_eq!(back, p);
}

#[test]
fn session_gap_notification_round_trip() {
    let p = SessionGapParams {
        session_id: "s1".into(),
        from_seq: 10,
        to_seq: 13,
        dropped_bytes: 2048,
    };
    let n = Notification::new(SessionGap::NAME, &p).unwrap();
    let Message::Notification(nn) = Message::from_slice(&serde_json::to_vec(&n).unwrap()).unwrap()
    else {
        panic!()
    };
    assert_eq!(nn.method, "session.gap");
    let back: SessionGapParams = nn.params_as().unwrap();
    assert_eq!(back, p);
}

#[test]
fn cron_fired_and_daemon_health_round_trip() {
    let cron = CronFiredParams {
        cron_id: "c1".into(),
        run_id: "r1".into(),
        fired_at: "2026-06-01T12:00:00Z".into(),
        status: "spawning".into(),
    };
    let n = Notification::new(CronFired::NAME, &cron).unwrap();
    let back: CronFiredParams = match Message::from_slice(&serde_json::to_vec(&n).unwrap()).unwrap()
    {
        Message::Notification(nn) => nn.params_as().unwrap(),
        _ => panic!(),
    };
    assert_eq!(back, cron);

    let health = DaemonHealthParams {
        queue_depth: 3,
        running: 7,
        errors_last_5m: 0,
    };
    let n = Notification::new(DaemonHealth::NAME, &health).unwrap();
    let back: DaemonHealthParams =
        match Message::from_slice(&serde_json::to_vec(&n).unwrap()).unwrap() {
            Message::Notification(nn) => nn.params_as().unwrap(),
            _ => panic!(),
        };
    assert_eq!(back, health);
}

// ----- Error-code mapping -----

/// Pins the [`ErrorKind`] → numeric-code table. The numeric values are
/// part of the protocol contract; reassigning one silently is a wire break.
#[test]
fn error_kind_to_code_table_is_pinned() {
    let table: &[(ErrorKind, i32)] = &[
        (ErrorKind::Parse, error_codes::PARSE_ERROR),
        (ErrorKind::InvalidRequest, error_codes::INVALID_REQUEST),
        (ErrorKind::MethodNotFound, error_codes::METHOD_NOT_FOUND),
        (ErrorKind::InvalidParams, error_codes::INVALID_PARAMS),
        (ErrorKind::Internal, error_codes::INTERNAL_ERROR),
        (ErrorKind::Server, error_codes::SERVER_ERROR_START),
        (ErrorKind::NotInitialized, error_codes::NOT_INITIALIZED),
        (
            ErrorKind::UnsupportedProtocolVersion,
            error_codes::UNSUPPORTED_PROTOCOL_VERSION,
        ),
        (ErrorKind::SessionNotFound, error_codes::SESSION_NOT_FOUND),
        (ErrorKind::WriterLocked, error_codes::WRITER_LOCKED),
        (ErrorKind::NotAttached, error_codes::NOT_ATTACHED),
        (
            ErrorKind::ReplayOutOfRange,
            error_codes::REPLAY_OUT_OF_RANGE,
        ),
        (ErrorKind::SessionBusy, error_codes::SESSION_BUSY),
        (ErrorKind::PayloadTooLarge, error_codes::PAYLOAD_TOO_LARGE),
        (
            ErrorKind::UnknownEventTopic,
            error_codes::UNKNOWN_EVENT_TOPIC,
        ),
        (
            ErrorKind::AdapterNotInstalled,
            error_codes::ADAPTER_NOT_INSTALLED,
        ),
        (
            ErrorKind::AdapterUnauthenticated,
            error_codes::ADAPTER_UNAUTHENTICATED,
        ),
        (
            ErrorKind::AdapterSpawnFailed,
            error_codes::ADAPTER_SPAWN_FAILED,
        ),
        (
            ErrorKind::AdapterProtocolDrift,
            error_codes::ADAPTER_PROTOCOL_DRIFT,
        ),
        (
            ErrorKind::AdapterUnsupportedOption,
            error_codes::ADAPTER_UNSUPPORTED_OPTION,
        ),
        (ErrorKind::StorageBusy, error_codes::STORAGE_BUSY),
        (ErrorKind::StorageConflict, error_codes::STORAGE_CONFLICT),
        (ErrorKind::StorageFailed, error_codes::STORAGE_FAILED),
        (ErrorKind::CronNotFound, error_codes::CRON_NOT_FOUND),
        (ErrorKind::CronInvalidExpr, error_codes::CRON_INVALID_EXPR),
        (
            ErrorKind::CronBudgetExceeded,
            error_codes::CRON_BUDGET_EXCEEDED,
        ),
        (ErrorKind::CronInvalidTz, error_codes::CRON_INVALID_TZ),
    ];
    for (k, expected) in table {
        assert_eq!(k.code(), *expected, "code drift for {:?}", k);
        // And the helper should produce that same code.
        let err = to_rpc_error(*k, "x", ()).unwrap();
        assert_eq!(err.code, *expected);
    }
    // Every business code must live in the documented range.
    for (k, code) in table {
        if matches!(
            k,
            ErrorKind::Parse
                | ErrorKind::InvalidRequest
                | ErrorKind::MethodNotFound
                | ErrorKind::InvalidParams
                | ErrorKind::Internal
                | ErrorKind::Server
        ) {
            continue;
        }
        assert!(
            *code <= error_codes::BUSINESS_ERROR_START,
            "{:?} ({code}) outside business range",
            k
        );
    }
}

#[test]
fn to_rpc_error_omits_data_when_unit_is_passed() {
    let err = to_rpc_error(ErrorKind::SessionNotFound, "no such id", ()).unwrap();
    let v = serde_json::to_value(&err).unwrap();
    assert!(
        v.get("data").is_none(),
        "data field must be absent for () payload, got {v:?}"
    );
}

#[test]
fn to_rpc_error_attaches_structured_data() {
    let err = to_rpc_error(
        ErrorKind::AdapterUnauthenticated,
        "log in first",
        json!({"docs_url": "https://example/login"}),
    )
    .unwrap();
    let v = serde_json::to_value(&err).unwrap();
    assert_eq!(v["code"], json!(error_codes::ADAPTER_UNAUTHENTICATED));
    assert_eq!(v["data"]["docs_url"], json!("https://example/login"));
}

// ----- Schema golden test -----

/// Asserts that the on-disk `docs/schema/*.json` files match what the
/// `la-proto-gen-schema` binary would emit right now. This is the "schema
/// follows code" invariant from architecture §12 — editing a typed struct
/// without re-running `gen_schema` (or vice versa) turns CI red.
///
/// To accept an intentional change, run:
///   `cargo run -p la-proto --bin la-proto-gen-schema`
/// and commit the updated files.
#[test]
fn schema_files_match_generated_output() {
    use std::path::PathBuf;

    // CARGO_MANIFEST_DIR is `crates/la-proto`; the schema lives at the
    // workspace root. Walking up two levels keeps the test working even if
    // invoked via `cargo test --workspace` from arbitrary cwd.
    let schema_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("docs")
        .join("schema");
    assert!(
        schema_dir.is_dir(),
        "expected schema dir at {}",
        schema_dir.display()
    );

    let mut expected = std::collections::BTreeMap::<String, String>::new();
    macro_rules! add_method {
        ($t:ty) => {{
            let safe = <$t>::NAME.replace('.', "__");
            let p = schema_for!(<$t as Method>::Params);
            let r = schema_for!(<$t as Method>::Result);
            expected.insert(
                format!("{safe}.params.schema.json"),
                serde_json::to_string_pretty(&p).unwrap() + "\n",
            );
            expected.insert(
                format!("{safe}.result.schema.json"),
                serde_json::to_string_pretty(&r).unwrap() + "\n",
            );
        }};
    }
    macro_rules! add_notif {
        ($t:ty) => {{
            let safe = <$t>::NAME.replace('.', "__");
            let p = schema_for!(<$t as NotificationMethod>::Params);
            expected.insert(
                format!("{safe}.params.schema.json"),
                serde_json::to_string_pretty(&p).unwrap() + "\n",
            );
        }};
    }

    add_method!(Initialize);
    add_method!(Shutdown);
    add_method!(SessionsList);
    add_method!(SessionsCreate);
    add_method!(SessionsAttach);
    add_method!(SessionsDetach);
    add_method!(SessionsWrite);
    add_method!(SessionsResize);
    add_method!(SessionsSignal);
    add_method!(SessionsArchive);
    add_method!(SessionsDelete);
    add_method!(SessionsImport);
    add_method!(SessionsReplay);
    add_method!(EventsSubscribe);

    add_notif!(SessionOutput);
    add_notif!(SessionStateNotice);
    add_notif!(SessionGap);
    add_notif!(CronFired);
    add_notif!(DaemonHealth);

    // 1. Every expected file exists with the expected bytes.
    let mut missing = Vec::new();
    let mut drifted = Vec::new();
    for (name, want) in &expected {
        let path = schema_dir.join(name);
        let got = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                missing.push(name.clone());
                continue;
            }
        };
        if got != *want {
            drifted.push(name.clone());
        }
    }

    // 2. No stray on-disk files we don't know about (catches a method
    //    being renamed / deleted in code but left orphaned on disk).
    let mut on_disk: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(&schema_dir).expect("read schema dir") {
        let entry = entry.unwrap();
        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with(".schema.json") {
                on_disk.insert(name.to_string());
            }
        }
    }
    let stray: Vec<String> = on_disk
        .iter()
        .filter(|n| !expected.contains_key(*n))
        .cloned()
        .collect();

    if !missing.is_empty() || !drifted.is_empty() || !stray.is_empty() {
        panic!(
            "docs/schema/ is out of sync with la-proto types.\n\
             Re-run: cargo run -p la-proto --bin la-proto-gen-schema\n\
             Missing:   {missing:?}\n\
             Drifted:   {drifted:?}\n\
             Orphaned:  {stray:?}"
        );
    }
}
