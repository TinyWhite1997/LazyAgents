//! Observability primitives shared by LazyAgents binaries.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::Utc;
use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::Layer;

static TRACE_SEQ: AtomicU64 = AtomicU64::new(1);
static EVENT_RING: OnceLock<RecentEvents> = OnceLock::new();
static METRICS: OnceLock<MetricsStore> = OnceLock::new();

/// Generate a 128-bit event identifier for log correlation.
///
/// This is intentionally local to the emitting event today; it is not a W3C
/// traceparent-compatible, end-to-end trace context propagation mechanism.
pub fn new_trace_id() -> String {
    let now = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Utc::now().timestamp_micros() * 1_000) as u128;
    let seq = TRACE_SEQ.fetch_add(1, Ordering::Relaxed) as u128;
    format!("{:032x}", (now << 64) ^ seq)
}

pub fn init_json_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let ring = EVENT_RING.get_or_init(RecentEvents::default).clone();
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(std::io::stderr)
        .with_ansi(false);
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(RecentEventLayer { ring })
        .with(fmt_layer);
    let _ = tracing::subscriber::set_global_default(subscriber);
}

pub fn install_crash_reporter(crash_dir: impl Into<PathBuf>) {
    let crash_dir = crash_dir.into();
    let ring = EVENT_RING.get_or_init(RecentEvents::default).clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = write_crash_report(&crash_dir, &ring, info);
        previous(info);
    }));
}

#[allow(deprecated)]
fn write_crash_report(
    crash_dir: &Path,
    ring: &RecentEvents,
    info: &std::panic::PanicInfo<'_>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(crash_dir)?;
    let ts = Utc::now();
    let path = crash_dir.join(format!("{}.json", ts.format("%Y%m%dT%H%M%S%.3fZ")));
    let payload = CrashReport {
        timestamp: ts.to_rfc3339(),
        thread: std::thread::current().name().map(str::to_string),
        message: panic_message(info),
        location: info.location().map(|loc| CrashLocation {
            file: loc.file().to_string(),
            line: loc.line(),
            column: loc.column(),
        }),
        recent_events: ring.snapshot(),
        upload_hint: "Attach this file to a LazyAgents issue if you choose to report the crash.",
    };
    let json = serde_json::to_vec_pretty(&payload)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

#[allow(deprecated)]
fn panic_message(info: &std::panic::PanicInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "panic payload is not a string".to_string()
    }
}

#[derive(Clone, Default)]
struct RecentEvents {
    inner: Arc<Mutex<VecDeque<EventRecord>>>,
}

impl RecentEvents {
    fn push(&self, event: EventRecord) {
        let mut guard = self.inner.lock().unwrap();
        if guard.len() == 100 {
            guard.pop_front();
        }
        guard.push_back(event);
    }

    fn snapshot(&self) -> Vec<EventRecord> {
        self.inner.lock().unwrap().iter().cloned().collect()
    }
}

struct RecentEventLayer {
    ring: RecentEvents,
}

impl<S> Layer<S> for RecentEventLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        self.ring.push(EventRecord {
            timestamp: Utc::now().to_rfc3339(),
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            fields: visitor.fields,
        });
    }
}

#[derive(Default)]
struct JsonVisitor {
    fields: Map<String, Value>,
}

impl tracing::field::Visit for JsonVisitor {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(field.name().to_string(), value.into());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(field.name().to_string(), value.into());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            Value::String(format!("{value:?}")),
        );
    }
}

#[derive(Clone, Serialize)]
struct EventRecord {
    timestamp: String,
    level: String,
    target: String,
    fields: Map<String, Value>,
}

#[derive(Serialize)]
struct CrashReport {
    timestamp: String,
    thread: Option<String>,
    message: String,
    location: Option<CrashLocation>,
    recent_events: Vec<EventRecord>,
    upload_hint: &'static str,
}

#[derive(Serialize)]
struct CrashLocation {
    file: String,
    line: u32,
    column: u32,
}

pub fn install_metrics_recorder() {
    let store = METRICS.get_or_init(MetricsStore::default).clone();
    let _ = metrics::set_global_recorder(store);
    describe_metrics();
}

pub fn render_prometheus() -> String {
    METRICS
        .get_or_init(MetricsStore::default)
        .render_prometheus()
}

fn describe_metrics() {
    metrics::describe_counter!(
        "lad_rpc_requests_total",
        Unit::Count,
        "Total JSON-RPC requests handled by lad, labelled by method and result."
    );
    metrics::describe_gauge!(
        "lad_session_active",
        Unit::Count,
        "Currently active sessions known to the daemon."
    );
    metrics::describe_counter!(
        "lad_session_output_bytes_total",
        Unit::Bytes,
        "Session output bytes delivered to attached clients."
    );
    metrics::describe_counter!(
        "lad_cron_runs_total",
        Unit::Count,
        "Cron runs by status; status=running marks the fire entry, all other statuses are terminal outcomes."
    );
    metrics::describe_histogram!(
        "lad_pty_spawn_duration_seconds",
        Unit::Seconds,
        "PTY/session spawn duration observed by the daemon."
    );
    metrics::describe_histogram!(
        "lad_storage_write_latency_seconds",
        Unit::Seconds,
        "Storage write latency for SQLite-backed mutations."
    );
}

#[derive(Clone, Default)]
struct MetricsStore {
    inner: Arc<Mutex<MetricsInner>>,
}

#[derive(Default)]
struct MetricsInner {
    descriptions: BTreeMap<String, MetricDescription>,
    counters: BTreeMap<MetricKey, u64>,
    gauges: BTreeMap<MetricKey, f64>,
    histograms: BTreeMap<MetricKey, HistogramValue>,
}

#[derive(Clone)]
struct MetricDescription {
    kind: &'static str,
    unit: Option<Unit>,
    description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct MetricKey {
    name: String,
    labels: Vec<(String, String)>,
}

impl MetricKey {
    fn from_key(key: &Key) -> Self {
        let mut labels: Vec<_> = key
            .labels()
            .map(|label| (label.key().to_string(), label.value().to_string()))
            .collect();
        labels.sort();
        Self {
            name: key.name().to_string(),
            labels,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct HistogramValue {
    count: u64,
    sum: f64,
}

impl Recorder for MetricsStore {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.describe(key, "counter", unit, description);
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.describe(key, "gauge", unit, description);
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.describe(key, "histogram", unit, description);
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(CounterHandle {
            store: self.clone(),
            key: MetricKey::from_key(key),
        }))
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(Arc::new(GaugeHandle {
            store: self.clone(),
            key: MetricKey::from_key(key),
        }))
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(Arc::new(HistogramHandle {
            store: self.clone(),
            key: MetricKey::from_key(key),
        }))
    }
}

impl MetricsStore {
    fn describe(
        &self,
        key: KeyName,
        kind: &'static str,
        unit: Option<Unit>,
        description: SharedString,
    ) {
        self.inner.lock().unwrap().descriptions.insert(
            key.as_str().to_string(),
            MetricDescription {
                kind,
                unit,
                description: description.to_string(),
            },
        );
    }

    fn render_prometheus(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let mut out = String::new();
        for (name, desc) in &inner.descriptions {
            out.push_str("# HELP ");
            out.push_str(name);
            out.push(' ');
            out.push_str(&escape_help(&desc.description));
            if let Some(unit) = desc.unit {
                out.push_str(" (");
                out.push_str(&format!("{unit:?}"));
                out.push(')');
            }
            out.push('\n');
            out.push_str("# TYPE ");
            out.push_str(name);
            out.push(' ');
            out.push_str(desc.kind);
            out.push('\n');
        }
        for (key, value) in &inner.counters {
            render_sample(&mut out, key, "", *value as f64);
        }
        for (key, value) in &inner.gauges {
            render_sample(&mut out, key, "", *value);
        }
        for (key, value) in &inner.histograms {
            render_sample_with_extra_label(
                &mut out,
                key,
                "_bucket",
                ("le", "+Inf"),
                value.count as f64,
            );
            render_sample(&mut out, key, "_sum", value.sum);
            render_sample(&mut out, key, "_count", value.count as f64);
        }
        out
    }
}

struct CounterHandle {
    store: MetricsStore,
    key: MetricKey,
}

impl CounterFn for CounterHandle {
    fn increment(&self, value: u64) {
        let mut inner = self.store.inner.lock().unwrap();
        *inner.counters.entry(self.key.clone()).or_default() += value;
    }

    fn absolute(&self, value: u64) {
        let mut inner = self.store.inner.lock().unwrap();
        inner.counters.insert(self.key.clone(), value);
    }
}

struct GaugeHandle {
    store: MetricsStore,
    key: MetricKey,
}

impl GaugeFn for GaugeHandle {
    fn increment(&self, value: f64) {
        let mut inner = self.store.inner.lock().unwrap();
        *inner.gauges.entry(self.key.clone()).or_default() += value;
    }

    fn decrement(&self, value: f64) {
        let mut inner = self.store.inner.lock().unwrap();
        *inner.gauges.entry(self.key.clone()).or_default() -= value;
    }

    fn set(&self, value: f64) {
        let mut inner = self.store.inner.lock().unwrap();
        inner.gauges.insert(self.key.clone(), value);
    }
}

struct HistogramHandle {
    store: MetricsStore,
    key: MetricKey,
}

impl HistogramFn for HistogramHandle {
    fn record(&self, value: f64) {
        let mut inner = self.store.inner.lock().unwrap();
        let entry = inner.histograms.entry(self.key.clone()).or_default();
        entry.count += 1;
        entry.sum += value;
    }
}

fn render_sample(out: &mut String, key: &MetricKey, suffix: &str, value: f64) {
    out.push_str(&key.name);
    out.push_str(suffix);
    if !key.labels.is_empty() {
        out.push('{');
        for (idx, (k, v)) in key.labels.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&escape_label(v));
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(&format!("{value}"));
    out.push('\n');
}

fn render_sample_with_extra_label(
    out: &mut String,
    key: &MetricKey,
    suffix: &str,
    extra: (&str, &str),
    value: f64,
) {
    out.push_str(&key.name);
    out.push_str(suffix);
    out.push('{');
    for (idx, (k, v)) in key.labels.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push_str("=\"");
        out.push_str(&escape_label(v));
        out.push('"');
    }
    if !key.labels.is_empty() {
        out.push(',');
    }
    out.push_str(extra.0);
    out.push_str("=\"");
    out.push_str(&escape_label(extra.1));
    out.push('"');
    out.push('}');
    out.push(' ');
    out.push_str(&format!("{value}"));
    out.push('\n');
}

fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_ids_are_128bit_hex() {
        let id = new_trace_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn metrics_render_prometheus_text() {
        install_metrics_recorder();
        metrics::counter!("lad_rpc_requests_total", "method" => "sessions.list", "result" => "ok")
            .increment(1);
        metrics::histogram!("lad_pty_spawn_duration_seconds").record(0.25);
        let rendered = render_prometheus();
        assert!(rendered.contains("lad_rpc_requests_total"));
        assert!(rendered.contains("method=\"sessions.list\""));
        assert!(rendered.contains("result=\"ok\""));
        assert!(rendered.contains("# TYPE lad_pty_spawn_duration_seconds histogram"));
        assert!(rendered.contains("lad_pty_spawn_duration_seconds_bucket{le=\"+Inf\"} 1"));
        assert!(rendered.contains("lad_pty_spawn_duration_seconds_sum 0.25"));
        assert!(rendered.contains("lad_pty_spawn_duration_seconds_count 1"));
        assert!(!rendered.contains("lad_pty_spawn_duration_seconds_max"));
    }

    #[test]
    fn panic_hook_writes_crash_report_with_recent_events() {
        let dir = std::env::temp_dir().join(format!("lazyagents-crash-test-{}", new_trace_id()));
        install_crash_reporter(&dir);
        tracing::error!(trace_id = %new_trace_id(), "before test panic");

        let _ = std::panic::catch_unwind(|| panic!("intentional crash report test"));

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("crash dir exists")
            .map(|e| e.expect("dir entry").path())
            .collect();
        assert_eq!(entries.len(), 1, "one crash report should be written");
        let text = std::fs::read_to_string(&entries[0]).expect("read crash report");
        assert!(text.contains("intentional crash report test"));
        assert!(text.contains("recent_events"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
