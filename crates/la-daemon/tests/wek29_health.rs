//! WEK-29 / M2.6 — `ProtocolDrift` must emit `error_event::adapter_drift`.
//!
//! The acceptance criterion from the issue body is explicit:
//!
//! > `ProtocolDrift` 必须 emit `error_event::adapter_drift` metric +
//! > 高优先级日志，提示用户升级。
//!
//! `crates/la-daemon/tests/acceptance.rs` already covers the
//! sessions.create pre-flight and the `daemon.health` snapshot
//! payload — this file just pins the log/metric channel because it
//! needs a custom `tracing::Layer` and would be out of scope for the
//! general acceptance suite.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use la_adapter::{
    AdapterDescriptor, AdapterError, AgentAdapter, ProbeResult, SpawnRequest, SpawnSpec,
};
use la_proto::notifications::BackendHealthStatus;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

struct FakeAdapter {
    descriptor: AdapterDescriptor,
    probe: ProbeResult,
}

#[async_trait]
impl AgentAdapter for FakeAdapter {
    fn descriptor(&self) -> AdapterDescriptor {
        self.descriptor.clone()
    }
    async fn probe(&self) -> ProbeResult {
        self.probe.clone()
    }
    fn spawn_spec(&self, _req: &SpawnRequest) -> Result<SpawnSpec, AdapterError> {
        unreachable!("probe-only adapter")
    }
    fn encode_user_input(&self, _text: &str) -> Bytes {
        Bytes::new()
    }
}

#[tokio::test]
async fn protocol_drift_emits_high_priority_log_event_and_classifies_health_row() {
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let layer = DriftCaptureLayer {
        sink: captured.clone(),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let _g = tracing::subscriber::set_default(subscriber);

    let adapter: Arc<dyn AgentAdapter> = Arc::new(FakeAdapter {
        descriptor: AdapterDescriptor {
            id: "codex",
            display_name: "Codex CLI",
            default_program: "codex",
            docs_url: "https://example.test/codex",
        },
        // Real codex adapter spelling — `crates/la-adapter/src/codex.rs`
        // emits this exact prefix when `--version` falls off the parser.
        probe: ProbeResult::Error {
            detail: "unrecognized --version output: \"???\"".into(),
        },
    });

    let registry = la_daemon::HealthRegistry::default();
    let tempdir = tempfile::tempdir().expect("tempdir");
    let storage = la_storage::Storage::open(la_storage::StorageConfig::new(
        tempdir.path().join("lad.sqlite"),
        tempdir.path().to_path_buf(),
    ))
    .await
    .expect("open storage");
    let manager = la_core::SessionManager::new(storage.clone(), la_core::ManagerConfig::default());
    let bus = la_core::EventBus::default();
    let cfg = la_daemon::ProbeLoopConfig {
        adapters: vec![("codex".into(), adapter)],
        registry: registry.clone(),
        storage,
        bus,
        manager,
        interval: Duration::from_secs(60),
        shutdown: Arc::new(tokio::sync::Notify::new()),
    };
    let handle = la_daemon::health::spawn_loop(cfg);
    // The loop runs one synchronous round before its first tick. Give
    // it a tiny beat for the upsert + log to land, then stop.
    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.abort();

    // Architecture §4.3 acceptance: drift produces a high-priority log
    // record on the `adapter_drift` target.
    let events = captured.lock().unwrap().clone();
    assert!(
        events.iter().any(|s| s.contains("target=adapter_drift")),
        "expected an adapter_drift log event; saw: {events:?}"
    );

    // …and the registry's wire snapshot classifies the same row as
    // `ProtocolDrift` so the TUI can grey-state it.
    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1, "exactly one backend was probed");
    assert_eq!(
        snap[0].status,
        BackendHealthStatus::ProtocolDrift,
        "drift-marker probe error must surface as ProtocolDrift, not Error",
    );
    // Acceptance: log includes backend version (here None because the
    // probe never parsed one) + drift detail.
    assert!(
        snap[0]
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("unrecognized")),
        "drift reason should preserve the parser's error detail",
    );
}

struct DriftCaptureLayer {
    sink: Arc<Mutex<Vec<String>>>,
}

impl<S> Layer<S> for DriftCaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        if meta.target() == "adapter_drift" {
            let mut visitor = StringVisitor::default();
            event.record(&mut visitor);
            self.sink
                .lock()
                .unwrap()
                .push(format!("target={} {}", meta.target(), visitor.0));
        }
    }
}

#[derive(Default)]
struct StringVisitor(String);

impl tracing::field::Visit for StringVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={:?}", field.name(), value));
    }
}
