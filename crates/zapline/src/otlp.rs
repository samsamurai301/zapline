//! Export `tracing` spans as OpenTelemetry traces over OTLP/HTTP (JSON),
//! with no OpenTelemetry SDK. Turned on by [`Telemetry::otlp`] or the
//! standard `OTEL_EXPORTER_OTLP_*` variables.
//!
//! [`Telemetry::otlp`]: crate::telemetry::Telemetry::otlp

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Spans sent per request to the collector.
const BATCH: usize = 512;
/// Spans waiting beyond this are dropped (the collector is slow or down).
const MAX_QUEUED: usize = 8192;
/// How often a partial batch is sent.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Events kept per span.
const MAX_EVENTS: usize = 64;

/// Where and how to send traces.
#[derive(Clone, Debug)]
pub(crate) struct OtlpConfig {
    /// The full traces URL (`.../v1/traces`).
    pub(crate) url: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) service_name: Option<String>,
}

impl OtlpConfig {
    /// `endpoint` is the collector's base URL (`http://localhost:4318`) or
    /// the full traces URL.
    pub(crate) fn new(endpoint: &str) -> Self {
        OtlpConfig {
            url: traces_url(endpoint),
            headers: env_headers(),
            service_name: None,
        }
    }

    /// From `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`, if either is set.
    pub(crate) fn from_env() -> Option<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        if let Some(url) = var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
            return Some(OtlpConfig {
                url,
                headers: env_headers(),
                service_name: None,
            });
        }
        var("OTEL_EXPORTER_OTLP_ENDPOINT").map(|e| Self::new(&e))
    }

    pub(crate) fn layer(self) -> OtlpLayer {
        OtlpLayer {
            queue: Exporter::start(self),
            ids: Ids::default(),
        }
    }
}

fn traces_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.ends_with("/v1/traces") {
        endpoint.to_owned()
    } else {
        format!("{endpoint}/v1/traces")
    }
}

/// `OTEL_EXPORTER_OTLP_HEADERS`: `key=value,key2=value2` (values may be
/// percent-encoded).
fn env_headers() -> Vec<(String, String)> {
    let raw = std::env::var("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default();
    parse_pairs(&raw)
}

fn parse_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let v = percent_encoding::percent_decode_str(v.trim()).decode_utf8_lossy();
            Some((k.trim().to_owned(), v.into_owned()))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// The `resource` of every batch: `service.name` and `OTEL_RESOURCE_ATTRIBUTES`.
fn resource(service_name: Option<String>) -> Value {
    let mut attrs = parse_pairs(&std::env::var("OTEL_RESOURCE_ATTRIBUTES").unwrap_or_default());
    let name = service_name
        .or_else(|| {
            std::env::var("OTEL_SERVICE_NAME")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            attrs
                .iter()
                .find(|(k, _)| k == "service.name")
                .map(|(_, v)| v.clone())
        })
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .file_stem()?
                .to_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "zapline-app".to_owned());
    attrs.retain(|(k, _)| k != "service.name");
    let mut list = vec![kv("service.name", &Attr::Str(name))];
    list.push(kv("telemetry.sdk.name", &Attr::Str("zapline".into())));
    list.push(kv("telemetry.sdk.language", &Attr::Str("rust".into())));
    list.push(kv(
        "telemetry.sdk.version",
        &Attr::Str(env!("CARGO_PKG_VERSION").into()),
    ));
    list.extend(attrs.into_iter().map(|(k, v)| kv(&k, &Attr::Str(v))));
    json!({ "attributes": list })
}

// ---------------------------------------------------------------------------
// Ids

/// Random-enough span and trace ids (not secrets: uniqueness is all that
/// matters).
struct Ids {
    seed: RandomState,
    counter: AtomicU64,
}

impl Default for Ids {
    fn default() -> Self {
        Ids {
            seed: RandomState::new(),
            counter: AtomicU64::new(0),
        }
    }
}

impl Ids {
    fn next(&self) -> u64 {
        let mut h = self.seed.build_hasher();
        h.write_u64(self.counter.fetch_add(1, Ordering::Relaxed));
        match h.finish() {
            0 => 1,
            n => n,
        }
    }

    fn trace(&self) -> u128 {
        (u128::from(self.next()) << 64) | u128::from(self.next())
    }
}

/// `traceparent: 00-{trace}-{span}-{flags}` -> (trace id, parent span id).
fn parse_traceparent(v: &str) -> Option<(u128, u64)> {
    let mut parts = v.trim().split('-');
    let (version, trace, span) = (parts.next()?, parts.next()?, parts.next()?);
    if version.len() != 2 || version == "ff" || trace.len() != 32 || span.len() != 16 {
        return None;
    }
    let trace = u128::from_str_radix(trace, 16).ok().filter(|t| *t != 0)?;
    let span = u64::from_str_radix(span, 16).ok().filter(|s| *s != 0)?;
    Some((trace, span))
}

// ---------------------------------------------------------------------------
// The layer

pub(crate) struct OtlpLayer {
    queue: Arc<Queue>,
    ids: Ids,
}

#[derive(Clone, Debug)]
enum Attr {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

struct SpanData {
    trace_id: u128,
    span_id: u64,
    parent_id: Option<u64>,
    name: &'static str,
    start: SystemTime,
    attrs: Vec<(&'static str, Attr)>,
    events: Vec<Value>,
}

struct Fields<'a>(&'a mut Vec<(&'static str, Attr)>);

impl Fields<'_> {
    fn put(&mut self, field: &Field, value: Attr) {
        match self.0.iter_mut().find(|(k, _)| *k == field.name()) {
            Some(slot) => slot.1 = value,
            None => self.0.push((field.name(), value)),
        }
    }
}

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Attr::Str(value.to_owned()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Attr::Int(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(
            field,
            i64::try_from(value).map_or(Attr::Str(value.to_string()), Attr::Int),
        );
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Attr::Float(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Attr::Bool(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Attr::Str(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, Attr::Str(format!("{value:?}")));
    }
}

fn nanos(t: SystemTime) -> String {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn kv(key: &str, value: &Attr) -> Value {
    let value = match value {
        Attr::Str(s) => json!({ "stringValue": s }),
        // OTLP/JSON writes 64-bit integers as strings.
        Attr::Int(n) => json!({ "intValue": n.to_string() }),
        Attr::Float(f) => json!({ "doubleValue": f }),
        Attr::Bool(b) => json!({ "boolValue": b }),
    };
    json!({ "key": key, "value": value })
}

impl<S> Layer<S> for OtlpLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Vec::new();
        attrs.record(&mut Fields(&mut fields));
        let parent = span.parent().and_then(|p| {
            p.extensions()
                .get::<SpanData>()
                .map(|d| (d.trace_id, d.span_id))
        });
        // A request continuing a trace from upstream (W3C `traceparent`).
        let remote = fields
            .iter()
            .position(|(k, _)| *k == "traceparent")
            .and_then(|i| {
                let (_, value) = fields.remove(i);
                match value {
                    Attr::Str(v) => parse_traceparent(&v),
                    _ => None,
                }
            });
        let (trace_id, parent_id) = match (parent, remote) {
            (Some((trace, span)), _) => (trace, Some(span)),
            (None, Some((trace, span))) => (trace, Some(span)),
            (None, None) => (self.ids.trace(), None),
        };
        span.extensions_mut().insert(SpanData {
            trace_id,
            span_id: self.ids.next(),
            parent_id,
            name: attrs.metadata().name(),
            start: SystemTime::now(),
            attrs: fields,
            events: Vec::new(),
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(data) = span.extensions_mut().get_mut::<SpanData>() {
                values.record(&mut Fields(&mut data.attrs));
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let mut ext = span.extensions_mut();
        let Some(data) = ext.get_mut::<SpanData>() else {
            return;
        };
        if data.events.len() >= MAX_EVENTS {
            return;
        }
        let mut fields = Vec::new();
        event.record(&mut Fields(&mut fields));
        let name = match fields.iter().position(|(k, _)| *k == "message") {
            Some(i) => match fields.remove(i).1 {
                Attr::Str(s) => s,
                other => format!("{other:?}"),
            },
            None => event.metadata().name().to_owned(),
        };
        let mut attrs: Vec<Value> = fields.iter().map(|(k, v)| kv(k, v)).collect();
        attrs.push(kv(
            "level",
            &Attr::Str(event.metadata().level().to_string()),
        ));
        data.events.push(json!({
            "timeUnixNano": nanos(SystemTime::now()),
            "name": name,
            "attributes": attrs,
        }));
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(data) = span.extensions_mut().remove::<SpanData>() else {
            return;
        };
        self.queue.push(to_otlp(data, span.metadata().target()));
    }
}

fn to_otlp(data: SpanData, target: &str) -> Value {
    let get = |key: &str| data.attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| v);
    let text = |key: &str| match get(key) {
        Some(Attr::Str(s)) => Some(s.as_str()),
        _ => None,
    };
    let request = data.name == "request" && get("http.method").is_some();
    // "GET /users/{id}" for requests, as OpenTelemetry names server spans.
    let name = match (request, text("http.method"), text("http.route")) {
        (true, Some(method), Some(route)) => format!("{method} {route}"),
        (true, Some(method), None) => method.to_owned(),
        _ => data.name.to_owned(),
    };
    let status = match get("http.status_code") {
        Some(Attr::Int(code)) if *code >= 500 => json!({ "code": 2 }),
        _ => json!({}),
    };
    let mut attrs: Vec<Value> = data
        .attrs
        .iter()
        .map(|(k, v)| {
            // OpenTelemetry's names for the request span's fields.
            let key = match (*k, request) {
                ("http.method", true) => "http.request.method",
                ("http.status_code", true) => "http.response.status_code",
                (k, _) => k,
            };
            kv(key, v)
        })
        .collect();
    attrs.push(kv("code.namespace", &Attr::Str(target.to_owned())));
    let mut span = json!({
        "traceId": format!("{:032x}", data.trace_id),
        "spanId": format!("{:016x}", data.span_id),
        "name": name,
        // SERVER for requests, INTERNAL otherwise.
        "kind": if request { 2 } else { 1 },
        "startTimeUnixNano": nanos(data.start),
        "endTimeUnixNano": nanos(SystemTime::now()),
        "attributes": attrs,
        "events": data.events,
        "status": status,
    });
    if let Some(parent) = data.parent_id {
        span["parentSpanId"] = json!(format!("{parent:016x}"));
    }
    span
}

// ---------------------------------------------------------------------------
// Export

struct Queue {
    spans: Mutex<Vec<Value>>,
    ready: Condvar,
    dropped: AtomicU64,
}

impl Queue {
    fn push(&self, span: Value) {
        let mut spans = self.spans.lock().unwrap_or_else(|e| e.into_inner());
        if spans.len() >= MAX_QUEUED {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        spans.push(span);
        if spans.len() >= BATCH {
            self.ready.notify_one();
        }
    }
}

struct Exporter;

impl Exporter {
    /// Start the export thread (its own small runtime, so it works whether
    /// or not the app's runtime is running yet) and return its queue.
    fn start(config: OtlpConfig) -> Arc<Queue> {
        let queue = Arc::new(Queue {
            spans: Mutex::default(),
            ready: Condvar::new(),
            dropped: AtomicU64::new(0),
        });
        let worker = queue.clone();
        let spawned = std::thread::Builder::new()
            .name("zapline-otlp".into())
            .spawn(move || export_loop(&worker, config));
        if let Err(e) = spawned {
            eprintln!("[zapline] warning: cannot start the OTLP exporter: {e}");
        }
        queue
    }
}

fn export_loop(queue: &Queue, config: OtlpConfig) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("[zapline] warning: cannot start the OTLP exporter runtime");
        return;
    };
    let resource = resource(config.service_name.clone());
    let mut warned = false;
    loop {
        let batch: Vec<Value> = {
            let mut spans = queue.spans.lock().unwrap_or_else(|e| e.into_inner());
            if spans.len() < BATCH {
                spans = queue
                    .ready
                    .wait_timeout(spans, FLUSH_EVERY)
                    .map(|(s, _)| s)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
            let n = spans.len().min(BATCH);
            spans.drain(..n).collect()
        };
        if batch.is_empty() {
            continue;
        }
        let body = json!({
            "resourceSpans": [{
                "resource": resource,
                "scopeSpans": [{
                    "scope": { "name": "zapline", "version": env!("CARGO_PKG_VERSION") },
                    "spans": batch,
                }],
            }],
        });
        let sent = rt.block_on(send(&config, body.to_string()));
        match sent {
            Ok(()) => warned = false,
            // Say it once per outage, not once per batch.
            Err(e) if !warned => {
                eprintln!(
                    "[zapline] warning: OTLP export to {} failed: {e}",
                    config.url
                );
                warned = true;
            }
            Err(_) => {}
        }
        let dropped = queue.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            eprintln!(
                "[zapline] warning: dropped {dropped} spans (the OTLP collector is too slow)"
            );
        }
    }
}

async fn send(config: &OtlpConfig, body: String) -> Result<(), String> {
    let mut req =
        http::Request::post(&config.url).header(http::header::CONTENT_TYPE, "application/json");
    for (k, v) in &config.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let req = req
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .map_err(|e| e.to_string())?;
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        crate::http_client::fetch(req, "the OTLP collector"),
    )
    .await
    .map_err(|_| "timed out".to_owned())?
    .map_err(|e| e.message().to_owned())?;
    if res.status.is_success() {
        Ok(())
    } else {
        Err(format!(
            "{} {}",
            res.status,
            String::from_utf8_lossy(&res.body)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_headers() {
        assert_eq!(traces_url("http://c:4318"), "http://c:4318/v1/traces");
        assert_eq!(traces_url("http://c:4318/"), "http://c:4318/v1/traces");
        assert_eq!(
            traces_url("https://x/otlp/v1/traces"),
            "https://x/otlp/v1/traces"
        );
        assert_eq!(
            parse_pairs("api-key=abc%3D, x-team = core ,bad"),
            [
                ("api-key".to_owned(), "abc=".to_owned()),
                ("x-team".to_owned(), "core".to_owned())
            ]
        );
    }

    #[test]
    fn traceparent() {
        let v = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(
            parse_traceparent(v),
            Some((0x4bf92f3577b34da6a3ce929d0e0e4736, 0x00f067aa0ba902b7))
        );
        for bad in [
            "",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f35-00f067aa0ba902b7-01",
        ] {
            assert_eq!(parse_traceparent(bad), None, "{bad}");
        }
    }
}
