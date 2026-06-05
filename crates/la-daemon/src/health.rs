//! Backend health tracker — adapter probing + `daemon.health` broadcast.
//!
//! WEK-29 / M2.6 错误分类（NotInstalled / Unauthenticated / ProtocolDrift）
//! + UI 灰态.
//!
//! Job description
//! ---------------
//!
//! 1. Right after [`crate::runtime::Daemon::bind`] returns, probe every
//!    registered adapter once so the very first `daemon.health` pulse
//!    carries an honest snapshot — the TUI starts grey-stating
//!    `NotInstalled` / `Unauthenticated` backends without waiting for the
//!    first 60 s refresh.
//! 2. Cache the latest [`ProbeResult`] per adapter inside [`BackendHealth`]
//!    and expose it through [`HealthRegistry::status_for`] so the
//!    dispatcher can short-circuit `sessions.create` with the right
//!    `-33101` / `-33102` code before ever touching the manager.
//! 3. Publish `daemon.health` on a steady cadence — once when the loop
//!    starts (so even a TUI that subscribes after the very first probe
//!    sees the snapshot) and then every [`DEFAULT_PROBE_INTERVAL`].
//!    Subscribed TUIs use the message to rebuild their sidebar.
//! 4. Whenever a probe surfaces [`ProbeResult::Error`] *with the marker
//!    string we treat as drift* (or an adapter raises
//!    [`AdapterError::ProtocolDrift`] during a probe), emit a
//!    `tracing::error!` record on `target = "adapter_drift"` so external
//!    metric pipelines can count it. The task description for WEK-29
//!    explicitly calls this out as the `error_event::adapter_drift`
//!    counter.
//!
//! The module is **pure infrastructure** — it does not know anything
//! about specific backends; everything is driven through the generic
//! [`AgentAdapter`] trait, so a future adapter slots in for free.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use la_adapter::{AgentAdapter, ProbeResult};
use la_core::{BusEvent, EventBus, SessionManager};
use la_proto::notifications::{
    BackendHealth as WireBackendHealth, BackendHealthStatus, DaemonHealthParams,
};
use la_storage::{BackendUpsert, Storage};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// How often the daemon re-probes every backend.
///
/// 60 s mirrors what §1.4 of the M2 architecture brief calls out: it
/// covers `claude login` flips, plus `codex` / `opencode` being installed
/// after `lad` booted, without restart. Probes themselves are bounded
/// internally (`async fn probe()` already returns within ~5 s by
/// convention), so the cadence is the rate-limit, not the latency.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Soft cap on a single probe — every adapter is expected to enforce its
/// own timeout, but the registry adds a belt-and-braces wrapper so a
/// pathologically broken adapter cannot wedge the whole health loop.
const PROBE_HARD_TIMEOUT: Duration = Duration::from_secs(10);

/// Lightweight cache of the latest probe outcome per adapter, keyed by
/// the registry id (same key the dispatcher uses).
///
/// Cheap to clone — internally an `Arc<RwLock<…>>`.
#[derive(Clone, Default)]
pub struct HealthRegistry {
    inner: Arc<RwLock<HashMap<String, BackendHealthEntry>>>,
}

/// Per-backend snapshot held in [`HealthRegistry`]. Mirrors the wire
/// [`WireBackendHealth`] but keeps the structured [`ProbeResult`] alongside
/// so callers can branch on the exact variant without re-parsing strings.
#[derive(Clone, Debug)]
pub struct BackendHealthEntry {
    pub id: String,
    pub display_name: String,
    pub docs_url: String,
    pub last_probe: ProbeResult,
    pub last_probed_at: String,
}

impl BackendHealthEntry {
    /// Convert to the wire shape consumed by `daemon.health` subscribers.
    pub fn to_wire(&self) -> WireBackendHealth {
        let (status, reason, docs_url, version) = match &self.last_probe {
            ProbeResult::Available { version } => (
                BackendHealthStatus::Available,
                None,
                None,
                Some(version.clone()),
            ),
            ProbeResult::NotInstalled { hint } => (
                BackendHealthStatus::NotInstalled,
                Some(hint.clone()),
                Some(self.docs_url.clone()),
                None,
            ),
            ProbeResult::Unauthenticated { docs_url } => (
                BackendHealthStatus::Unauthenticated,
                Some(format!("not logged in; see {docs_url}")),
                Some(if docs_url.is_empty() {
                    self.docs_url.clone()
                } else {
                    docs_url.clone()
                }),
                None,
            ),
            ProbeResult::Error { detail } if looks_like_drift(detail) => (
                BackendHealthStatus::ProtocolDrift,
                Some(detail.clone()),
                Some(self.docs_url.clone()),
                None,
            ),
            ProbeResult::Error { detail } => {
                (BackendHealthStatus::Error, Some(detail.clone()), None, None)
            }
        };
        WireBackendHealth {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            status,
            version,
            reason,
            docs_url,
            last_probed_at: self.last_probed_at.clone(),
        }
    }
}

/// Returns true when the cached probe state means `sessions.create` for
/// this backend should be refused before we ever touch the adapter.
pub fn is_blocking(probe: &ProbeResult) -> bool {
    matches!(
        probe,
        ProbeResult::NotInstalled { .. } | ProbeResult::Unauthenticated { .. }
    )
}

/// Returns true when a generic [`ProbeResult::Error`] detail self-
/// identifies as protocol drift.
///
/// We classify three keyword families as drift, all of them produced by
/// the shipped adapters when their `--version` parser falls over:
///   * `"drift"` / `"protocol drift"` — adapters can spell it out.
///   * `"unrecognized"` — `codex.rs` / `opencode.rs` emit
///     `"unrecognized --version output: …"` when the regex misses.
///   * `"could not parse"` — generic parser failure phrasing.
///
/// Treating each of these as drift means the health loop fires the
/// `adapter_drift` metric (architecture §4.3 acceptance) regardless of
/// which specific adapter regressed. Adapters that catch drift
/// explicitly can also raise `AdapterError::ProtocolDrift`; that path
/// reaches us via the spawn flow, not the probe loop.
fn looks_like_drift(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("drift")
        || lower.contains("protocol drift")
        || lower.contains("unrecognized")
        || lower.contains("could not parse")
        || lower.contains("unexpected --version")
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the cached probe outcome for `backend_id`. Returns `None`
    /// when no probe has run yet (first 60 s after boot for a brand-new
    /// adapter).
    pub async fn probe_for(&self, backend_id: &str) -> Option<ProbeResult> {
        self.inner
            .read()
            .await
            .get(backend_id)
            .map(|e| e.last_probe.clone())
    }

    /// Returns the cached wire snapshot for all backends, sorted by id
    /// (deterministic so TUI rendering doesn't jitter).
    pub async fn snapshot(&self) -> Vec<WireBackendHealth> {
        let map = self.inner.read().await;
        let mut out: Vec<_> = map.values().map(BackendHealthEntry::to_wire).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    async fn upsert(&self, entry: BackendHealthEntry) {
        let mut map = self.inner.write().await;
        map.insert(entry.id.clone(), entry);
    }
}

/// Inputs for [`run_probe_loop`]. Spelled out so tests can inject a
/// fake storage / shorter interval / different bus, and so the runtime
/// doesn't need to import everything just to spawn the loop.
pub struct ProbeLoopConfig {
    pub adapters: Vec<(String, Arc<dyn AgentAdapter>)>,
    pub registry: HealthRegistry,
    pub storage: Storage,
    pub bus: EventBus,
    /// Manager handle used to source the current running-session count
    /// for the `daemon.health.running` field. The loop calls
    /// `manager.active_count().await` once per pulse.
    pub manager: SessionManager,
    /// Interval between probe rounds. Tests usually shrink this.
    pub interval: Duration,
    /// Shutdown signal — when notified the loop exits cleanly.
    pub shutdown: Arc<tokio::sync::Notify>,
}

/// Run the probe + broadcast loop until `shutdown` fires.
///
/// The first iteration runs synchronously *before* the timer starts so
/// the initial `daemon.health` always reflects a real probe, not the
/// `available=true` placeholder the runtime upserts at bind time.
pub async fn run_probe_loop(cfg: ProbeLoopConfig) {
    // First round, inline so `daemon.health` is honest from message #1.
    probe_once_and_broadcast(&cfg).await;

    let mut ticker = tokio::time::interval(cfg.interval);
    // First tick fires immediately on `interval` — skip it; we already
    // ran the synchronous round above.
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            _ = cfg.shutdown.notified() => break,
            _ = ticker.tick() => {
                probe_once_and_broadcast(&cfg).await;
            }
        }
    }
}

async fn probe_once_and_broadcast(cfg: &ProbeLoopConfig) {
    let mut wire_entries: Vec<WireBackendHealth> = Vec::with_capacity(cfg.adapters.len());
    for (id, adapter) in &cfg.adapters {
        let entry = probe_one(id, adapter.as_ref()).await;
        record_entry(&cfg.registry, &cfg.storage, &entry).await;
        wire_entries.push(entry.to_wire());
    }
    wire_entries.sort_by(|a, b| a.id.cmp(&b.id));

    let running = cfg.manager.active_count().await as u32;
    metrics::gauge!("lad_session_active").set(running as f64);
    let params = DaemonHealthParams {
        // M3 will populate this when the scheduler lands; until then we
        // honestly report zero. Keeping the field on the wire shape
        // stable matters more than guessing a placeholder.
        queue_depth: 0,
        running,
        // Treat any non-Available probe in *this* round as a counted
        // error; the value is "errors observed in the last 5m" per the
        // existing field doc, but absent a real metric pipeline the
        // round count is the next-best proxy and the TUI's status-bar
        // dot only branches on `> 0`.
        errors_last_5m: wire_entries
            .iter()
            .filter(|b| b.status != BackendHealthStatus::Available)
            .count() as u32,
        backends: wire_entries,
        // S1 / WEK-73: surface the supervising service manager so the
        // TUI status bar can label the daemon's origin without
        // querying systemd/launchd itself. `LAZYAGENTS_MANAGED_BY` is
        // set by the service unit templates we install in M4.1.
        managed_by: crate::install::detect_running_service(),
    };
    cfg.bus.publish(BusEvent::DaemonHealth(params));
}

async fn probe_one(id: &str, adapter: &dyn AgentAdapter) -> BackendHealthEntry {
    let desc = adapter.descriptor();
    let probe = match tokio::time::timeout(PROBE_HARD_TIMEOUT, adapter.probe()).await {
        Ok(p) => p,
        Err(_) => ProbeResult::Error {
            detail: format!("probe timed out after {:?}", PROBE_HARD_TIMEOUT),
        },
    };

    // Emit the `adapter_drift` metric/log as soon as we see drift —
    // both the parsed `ProtocolDrift` flavour an adapter could raise as
    // `AdapterError::ProtocolDrift` (we surface that variant via the
    // `Error { detail }` path; see `looks_like_drift`) and any
    // `Error { detail }` whose detail self-identifies as drift.
    //
    // M4.5 / WEK-75 — A9: the canonical surface is now the
    // `lad_adapter_drift_total{backend}` counter (architecture §9.3
    // pinned metric naming table). The structured log on
    // `target = "adapter_drift"` is preserved so existing log-side
    // collectors (Loki dashboards, the WEK-29 acceptance test) keep
    // working — it is no longer the only surface.
    if let ProbeResult::Error { detail } = &probe {
        if looks_like_drift(detail) {
            metrics::counter!("lad_adapter_drift_total", "backend" => id.to_string()).increment(1);
            // High-priority, machine-parseable record. The `target`
            // is the metric name per task description.
            tracing::error!(
                target: "adapter_drift",
                backend = %id,
                version = tracing::field::Empty,
                detail = %detail,
                "backend protocol drift detected",
            );
        }
    }
    // Log every other failure too — without the `adapter_drift` target
    // so the metric counter doesn't fire, but with enough fields that
    // operators see why a backend grey-stated.
    match &probe {
        ProbeResult::Available { version } => {
            tracing::debug!(
                backend = %id,
                version = %version,
                "adapter probe ok",
            );
        }
        ProbeResult::NotInstalled { hint } => {
            tracing::warn!(backend = %id, hint = %hint, "adapter probe: not installed");
        }
        ProbeResult::Unauthenticated { docs_url } => {
            tracing::warn!(
                backend = %id,
                docs_url = %docs_url,
                "adapter probe: not authenticated",
            );
        }
        ProbeResult::Error { detail } if !looks_like_drift(detail) => {
            tracing::warn!(backend = %id, detail = %detail, "adapter probe: error");
        }
        _ => {}
    }

    BackendHealthEntry {
        id: id.to_string(),
        display_name: desc.display_name.to_string(),
        docs_url: desc.docs_url.to_string(),
        last_probe: probe,
        last_probed_at: now_rfc3339(),
    }
}

async fn record_entry(registry: &HealthRegistry, storage: &Storage, entry: &BackendHealthEntry) {
    let version = match &entry.last_probe {
        ProbeResult::Available { version } => Some(version.clone()),
        _ => None,
    };
    let available = matches!(entry.last_probe, ProbeResult::Available { .. });
    // Mirror the snapshot into SQLite so `sessions.list` joins still
    // see the latest classified state across restarts. We swallow the
    // upsert error: it is best-effort metadata, not the source of
    // truth (which is the in-memory `HealthRegistry`).
    if let Err(err) = storage
        .backends()
        .upsert(BackendUpsert {
            id: &entry.id,
            display_name: &entry.display_name,
            version: version.as_deref(),
            available,
        })
        .await
    {
        tracing::warn!(backend = %entry.id, %err, "failed to upsert backend probe to storage");
    }
    registry.upsert(entry.clone()).await;
}

/// Spawn [`run_probe_loop`] on the current tokio runtime and return its
/// join handle. The runtime owns the handle so graceful shutdown awaits
/// the loop too — the loop respects `cfg.shutdown` and exits within one
/// tick.
pub fn spawn_loop(cfg: ProbeLoopConfig) -> JoinHandle<()> {
    tokio::spawn(async move { run_probe_loop(cfg).await })
}

/// Minimal RFC3339 timestamp formatter. Avoids a `chrono` dependency for
/// what is effectively a debug field on `daemon.health`.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // y/m/d/h/m/s from a unix timestamp without chrono.
    // Good-enough algorithm — leap seconds ignored; the field is
    // advisory, not load-bearing.
    let (year, month, day, hour, minute, second) = unix_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn unix_to_ymdhms(unix: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (unix / 86_400) as i64;
    let secs_today = (unix % 86_400) as u32;
    let hour = secs_today / 3600;
    let minute = (secs_today % 3600) / 60;
    let second = secs_today % 60;

    // Days from 1970-01-01 — algorithm by Howard Hinnant
    // (https://howardhinnant.github.io/date_algorithms.html), the
    // shortest known correct one.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = (y + i64::from(m <= 2)) as i32;
    (y, m as u32, d as u32, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use la_adapter::{AdapterDescriptor, SpawnRequest, SpawnSpec};
    use std::path::PathBuf;

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
        fn spawn_spec(&self, _req: &SpawnRequest) -> Result<SpawnSpec, la_adapter::AdapterError> {
            unreachable!("not used by health tests")
        }
        fn encode_user_input(&self, _text: &str) -> Bytes {
            Bytes::new()
        }
    }

    fn fake(id: &'static str, name: &'static str, probe: ProbeResult) -> Arc<dyn AgentAdapter> {
        Arc::new(FakeAdapter {
            descriptor: AdapterDescriptor {
                id,
                display_name: name,
                default_program: id,
                docs_url: "https://example.com/docs",
            },
            probe,
        })
    }

    #[test]
    fn looks_like_drift_recognises_common_phrasing() {
        assert!(looks_like_drift("backend protocol drift: weird version"));
        assert!(looks_like_drift("Drift in version output"));
        // Real adapter strings: `codex.rs` / `opencode.rs` build a probe
        // error of the form "unrecognized --version output: ...". WEK-29
        // acceptance is that we classify these as ProtocolDrift, not as
        // a generic Error — otherwise `adapter_drift` never fires for
        // the most common drift symptom.
        assert!(looks_like_drift("unrecognized --version output: \"???\""));
        assert!(looks_like_drift("could not parse version line"));
        assert!(!looks_like_drift("timeout while probing"));
        assert!(!looks_like_drift("ENOENT"));
    }

    #[test]
    fn entry_to_wire_maps_every_probe_variant() {
        let cases = [
            (
                ProbeResult::Available {
                    version: "1.2.3".into(),
                },
                BackendHealthStatus::Available,
                Some("1.2.3"),
                None::<&str>,
            ),
            (
                ProbeResult::NotInstalled {
                    hint: "not on PATH".into(),
                },
                BackendHealthStatus::NotInstalled,
                None,
                Some("not on PATH"),
            ),
            (
                ProbeResult::Unauthenticated {
                    docs_url: "https://login/example".into(),
                },
                BackendHealthStatus::Unauthenticated,
                None,
                Some("not logged in; see https://login/example"),
            ),
            (
                ProbeResult::Error {
                    detail: "drift in version output".into(),
                },
                BackendHealthStatus::ProtocolDrift,
                None,
                Some("drift in version output"),
            ),
            (
                ProbeResult::Error {
                    detail: "ECONNREFUSED".into(),
                },
                BackendHealthStatus::Error,
                None,
                Some("ECONNREFUSED"),
            ),
        ];
        for (probe, status, version, reason) in cases {
            let entry = BackendHealthEntry {
                id: "foo".into(),
                display_name: "Foo".into(),
                docs_url: "https://docs/foo".into(),
                last_probe: probe.clone(),
                last_probed_at: "now".into(),
            };
            let wire = entry.to_wire();
            assert_eq!(
                wire.status, status,
                "wrong status for {:?}: got {:?}",
                probe, wire.status
            );
            assert_eq!(
                wire.version.as_deref(),
                version,
                "wrong version: {:?}",
                probe
            );
            assert_eq!(wire.reason.as_deref(), reason, "wrong reason: {:?}", probe);
        }
    }

    #[test]
    fn is_blocking_only_for_install_and_auth_states() {
        assert!(is_blocking(&ProbeResult::NotInstalled {
            hint: "n/a".into()
        }));
        assert!(is_blocking(&ProbeResult::Unauthenticated {
            docs_url: "n/a".into()
        }));
        assert!(!is_blocking(&ProbeResult::Available {
            version: "1".into()
        }));
        assert!(!is_blocking(&ProbeResult::Error {
            detail: "n/a".into()
        }));
    }

    #[tokio::test]
    async fn probe_once_caches_results_and_publishes_health() {
        let registry = HealthRegistry::default();
        let storage = la_storage::Storage::open(la_storage::StorageConfig::new(
            PathBuf::from(":memory:"),
            std::env::temp_dir(),
        ))
        .await
        .expect("open in-memory storage");
        let manager =
            la_core::SessionManager::new(storage.clone(), la_core::ManagerConfig::default());
        let bus = manager.bus();
        let mut rx = bus.subscribe();

        let cfg = ProbeLoopConfig {
            adapters: vec![
                (
                    "claude".to_string(),
                    fake(
                        "claude",
                        "Claude Code",
                        ProbeResult::Available {
                            version: "2.1.158".into(),
                        },
                    ),
                ),
                (
                    "codex".to_string(),
                    fake(
                        "codex",
                        "Codex",
                        ProbeResult::NotInstalled {
                            hint: "not on PATH".into(),
                        },
                    ),
                ),
                (
                    "opencode".to_string(),
                    fake(
                        "opencode",
                        "OpenCode",
                        ProbeResult::Error {
                            detail: "drift in stdout".into(),
                        },
                    ),
                ),
            ],
            registry: registry.clone(),
            storage,
            bus,
            manager,
            interval: Duration::from_secs(60),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        };
        probe_once_and_broadcast(&cfg).await;

        // Cache contents
        let snapshot = registry.snapshot().await;
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].id, "claude");
        assert_eq!(snapshot[0].status, BackendHealthStatus::Available);
        assert_eq!(snapshot[1].status, BackendHealthStatus::NotInstalled);
        assert_eq!(snapshot[2].status, BackendHealthStatus::ProtocolDrift);

        // Wire broadcast
        let evt = rx.recv().await.expect("daemon.health published");
        match evt {
            BusEvent::DaemonHealth(p) => {
                assert_eq!(p.backends.len(), 3);
                assert_eq!(
                    p.errors_last_5m, 2,
                    "two non-Available backends contribute to the error count",
                );
            }
            other => panic!("expected DaemonHealth, got {:?}", other),
        }
    }

    /// `WEK-29` acceptance: the `error_event::adapter_drift` metric must
    /// fire on every detected drift, and the record must carry the
    /// `backend` + `detail` fields so an operator can identify which
    /// adapter needs upgrading without grepping daemon logs by ID.
    #[tokio::test]
    async fn probe_emits_adapter_drift_record_on_protocol_drift() {
        use std::sync::Mutex;
        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for CaptureWriter {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = CaptureWriter(buffer.clone());
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_target(true)
            .with_ansi(false);
        let subscriber = tracing_subscriber::registry().with(layer);

        let drift_adapter = fake(
            "opencode",
            "OpenCode",
            ProbeResult::Error {
                detail: "backend protocol drift: unknown event 'foo' from v9.9".into(),
            },
        );
        // Use the dispatcher API that lets us scope a subscriber to a
        // single async block — needed because the surrounding
        // #[tokio::test] already installed the default.
        let _entry = tracing::subscriber::set_default(subscriber);
        let _ = probe_one("opencode", drift_adapter.as_ref()).await;

        let captured = String::from_utf8(buffer.lock().unwrap().clone()).expect("utf8 log");
        assert!(
            captured.contains("adapter_drift"),
            "captured log must include the `adapter_drift` target so external metric \
             pipelines can count drift events:\n{captured}",
        );
        assert!(
            captured.contains("backend=\"opencode\"") || captured.contains("backend=opencode"),
            "captured log must include the backend id:\n{captured}",
        );
        assert!(
            captured.contains("backend protocol drift"),
            "captured log must include the drift detail:\n{captured}",
        );
    }
}
