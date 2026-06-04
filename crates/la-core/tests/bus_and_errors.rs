//! Pin-down tests for the layers `la-core` does NOT spawn PTYs in:
//! the [`EventBus`] broadcast surface and [`CoreError`] → wire-code
//! mapping. Kept separate from `lifecycle.rs` so a slow PTY test failure
//! doesn't hide a mapping regression.

use la_core::{BusEvent, CoreError, EventBus, Topic};
use la_proto::error_codes;
use la_proto::methods::EventTopic;
use la_proto::notifications::{DaemonHealthParams, SessionGapParams, SessionStateParams};

#[tokio::test]
async fn bus_broadcasts_to_all_subscribers() {
    let bus = EventBus::default();
    let mut rx1 = bus.subscribe();
    let mut rx2 = bus.subscribe();

    let sent = bus.publish(BusEvent::DaemonHealth(DaemonHealthParams {
        queue_depth: 1,
        running: 2,
        errors_last_5m: 0,
        backends: Vec::new(),
        managed_by: None,
    }));
    assert_eq!(sent, 2, "both subs should receive");

    let e1 = rx1.recv().await.expect("rx1 recv");
    let e2 = rx2.recv().await.expect("rx2 recv");
    assert!(matches!(e1, BusEvent::DaemonHealth(_)));
    assert!(matches!(e2, BusEvent::DaemonHealth(_)));
}

#[tokio::test]
async fn bus_publish_without_subscribers_is_ok() {
    let bus = EventBus::default();
    let sent = bus.publish(BusEvent::SessionGap(SessionGapParams {
        session_id: "s".into(),
        from_seq: 1,
        to_seq: 2,
        dropped_bytes: 64,
    }));
    assert_eq!(sent, 0, "no subscribers ⇒ 0 recipients, not an error");
}

#[test]
fn topic_roundtrip_through_proto() {
    let topics = [
        Topic::SessionState,
        Topic::SessionGap,
        Topic::CronFired,
        Topic::DaemonHealth,
    ];
    for t in topics {
        let back = Topic::from_proto(t.as_proto()).expect("roundtrip");
        assert_eq!(t, back);
    }
    // SessionOutput is intentionally not on the bus — confirm it doesn't
    // accidentally appear in the conversion.
    assert!(Topic::from_proto(EventTopic::SessionOutput).is_none());
}

#[test]
fn bus_event_topic_tag_matches_variant() {
    let e = BusEvent::SessionState(SessionStateParams {
        session_id: "s".into(),
        state: la_proto::methods::SessionState::Running,
        exit_code: None,
        reason: None,
    });
    assert_eq!(e.topic(), Topic::SessionState);
}

#[test]
fn error_kind_maps_to_business_codes() {
    use la_adapter::AdapterError;
    use la_storage::StorageError;
    let cases: Vec<(CoreError, i32)> = vec![
        (
            CoreError::Adapter(AdapterError::NotInstalled { hint: "x".into() }),
            error_codes::ADAPTER_NOT_INSTALLED,
        ),
        (
            CoreError::Adapter(AdapterError::Unauthenticated {
                docs_url: "x".into(),
            }),
            error_codes::ADAPTER_UNAUTHENTICATED,
        ),
        (
            CoreError::Adapter(AdapterError::ProtocolDrift { detail: "x".into() }),
            error_codes::ADAPTER_PROTOCOL_DRIFT,
        ),
        (
            CoreError::SessionNotFound("s".into()),
            error_codes::SESSION_NOT_FOUND,
        ),
        (
            CoreError::WriterLocked { holder: 1 },
            error_codes::WRITER_LOCKED,
        ),
        (CoreError::NotAttached, error_codes::NOT_ATTACHED),
        (CoreError::SessionBusy, error_codes::SESSION_BUSY),
        (
            CoreError::Storage(StorageError::Busy { attempts: 5 }),
            error_codes::STORAGE_BUSY,
        ),
        (CoreError::Internal("x".into()), error_codes::INTERNAL_ERROR),
    ];
    for (err, expected) in cases {
        let code = err.kind().code();
        assert_eq!(
            code, expected,
            "wrong wire code for {err:?}: got {code}, want {expected}",
        );
    }
}

/// `WEK-29` 验收 — "错误码段映射正确"：every `AdapterError` variant must
/// land inside the dedicated `-33100..-33199` adapter business range.
/// A new variant that misses this lane (e.g. accidentally falls through
/// to `Internal`) is a contract regression — the dispatcher would no
/// longer be able to disambiguate the failure surface for the TUI's
/// grey-state renderer.
#[test]
fn adapter_error_variants_all_live_in_the_adapter_code_segment() {
    use la_adapter::AdapterError;
    let cases: Vec<CoreError> = vec![
        CoreError::Adapter(AdapterError::NotInstalled { hint: "x".into() }),
        CoreError::Adapter(AdapterError::Unauthenticated {
            docs_url: "x".into(),
        }),
        CoreError::Adapter(AdapterError::ProtocolDrift { detail: "x".into() }),
        CoreError::Adapter(AdapterError::UnsupportedOption { name: "x".into() }),
        CoreError::Adapter(AdapterError::SpawnFailed(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ))),
    ];
    for err in cases {
        let code = err.kind().code();
        assert!(
            (-33199..=-33101).contains(&code),
            "{err:?} mapped to {code}; expected somewhere in the adapter -33101..-33199 segment",
        );
    }
}

/// `WEK-29` — `AdapterError::Transient` is intentionally NOT in the
/// adapter segment; it represents a recoverable internal hiccup and
/// folds into the generic `Internal` code (`-32603`). Pinning that
/// choice so a future refactor doesn't silently promote it into the
/// adapter range and start triggering grey-state in the TUI for what
/// should be a transient retryable failure.
#[test]
fn adapter_error_transient_folds_into_internal_not_adapter_segment() {
    use la_adapter::AdapterError;
    let err = CoreError::Adapter(AdapterError::Transient("temporary".into()));
    assert_eq!(err.kind().code(), error_codes::INTERNAL_ERROR);
}
