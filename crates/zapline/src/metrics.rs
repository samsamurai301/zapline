//! Prometheus metrics: HTTP request metrics, process metrics and a registry
//! for your own counters, gauges and histograms. No extra dependencies.
//!
//! ```no_run
//! use zapline::prelude::*;
//! use zapline::metrics::{self, Metrics};
//!
//! async fn signup() -> &'static str {
//!     metrics::registry()
//!         .counter("signups_total", "New accounts.", &["plan"])
//!         .with_labels(&["free"])
//!         .inc();
//!     "welcome"
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .post("/signup", signup)
//!         .plugin(Metrics::new()) // GET /metrics
//!         .run("0.0.0.0:8080")
//! }
//! ```
//!
//! The plugin exports:
//!
//! | Series | Type | Labels |
//! |---|---|---|
//! | `http_requests_total` | counter | `method`, `route`, `status` |
//! | `http_request_duration_seconds` | histogram | `method`, `route` |
//! | `http_requests_in_flight` | gauge | |
//! | `process_*` (see [`ProcessCollector`]) | | |
//!
//! `route` is always the route *template* (`/users/{id}`), never the raw
//! path, so cardinality is bounded by your route table. Requests that match
//! no route (404/405 from the router or fallback) are counted with
//! `route="unmatched"`. Non-standard methods are reported as `_OTHER` unless
//! the route was registered with that method.

mod process;
mod registry;
mod requests;

use std::sync::Arc;

use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::HeaderValue;

pub use self::process::ProcessCollector;
pub use self::registry::{
    is_valid_label_name, is_valid_metric_name, registry, Collector, Counter, Encoder, Gauge,
    Histogram, HistogramTimer, MetricKind, Registry, DEFAULT_BUCKETS, MAX_SERIES_PER_FAMILY,
};
use self::requests::{GlobalLayer, HttpConfig, HttpMetrics, PathPattern, RouteLayer};
use crate::app::{App, Plugin};
use crate::response::{IntoResponse, Response};
use crate::router::Routes;

/// Content type of the Prometheus text exposition format 0.0.4.
pub const CONTENT_TYPE_TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The metrics plugin: instruments every route and serves `GET /metrics`.
///
/// ```
/// use std::time::Duration;
/// use zapline::prelude::*;
/// use zapline::metrics::{Metrics, Registry};
///
/// let custom = Registry::new();
/// let app = App::new()
///     .get("/users/{id}", || async { "ada" })
///     .plugin(
///         Metrics::new()
///             .path("/internal/metrics")
///             .prefix("api")
///             .buckets([0.001, 0.01, 0.1, 1.0])
///             .exclude_paths(["/health/*"])
///             .registry(custom),
///     );
/// ```
///
/// Installs:
/// - a route middleware ([`App::wrap_route`]) that records each matched
///   request under its route template;
/// - unless [`track_unmatched(false)`](Self::track_unmatched), a global
///   middleware ([`App::wrap`]) that counts requests matching no route and
///   tracks in-flight requests. Install `Metrics` *after*
///   [`App::error_handler`] (and other middleware that replaces responses)
///   so it sees the router's own 404/405 responses;
/// - the scrape endpoint (unless [`endpoint(false)`](Self::endpoint)) and a
///   [`MetricsHandle`] as app state for custom exposition.
#[must_use = "install it with `App::plugin`"]
pub struct Metrics {
    path: String,
    endpoint: bool,
    buckets: Vec<f64>,
    prefix: String,
    exclude: Vec<String>,
    registry: Option<Registry>,
    track_unmatched: bool,
    process: bool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Defaults: `GET /metrics`, prefix `http`, [`DEFAULT_BUCKETS`], the
    /// global [`registry()`], process metrics on, unmatched tracking on.
    pub fn new() -> Self {
        Metrics {
            path: "/metrics".into(),
            endpoint: true,
            buckets: DEFAULT_BUCKETS.to_vec(),
            prefix: "http".into(),
            exclude: Vec::new(),
            registry: None,
            track_unmatched: true,
            process: true,
        }
    }

    /// Path of the scrape endpoint (default `/metrics`).
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Whether to register the scrape endpoint (default `true`). Without it,
    /// expose metrics yourself through the [`MetricsHandle`] state.
    pub fn endpoint(mut self, enabled: bool) -> Self {
        self.endpoint = enabled;
        self
    }

    /// Latency histogram bucket upper bounds in seconds.
    ///
    /// # Panics
    /// At install time if the bounds are not finite and strictly increasing.
    pub fn buckets(mut self, buckets: impl IntoIterator<Item = f64>) -> Self {
        self.buckets = buckets.into_iter().collect();
        self
    }

    /// Metric name prefix (default `http`, giving `http_requests_total`).
    /// An empty prefix gives `requests_total`.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Route templates (or, for unmatched requests, raw paths) not to
    /// record. A trailing `*` matches a prefix: `"/internal/*"`.
    pub fn exclude_paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exclude.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Render this registry's custom metrics instead of the global
    /// [`registry()`].
    pub fn registry(mut self, registry: Registry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Count requests matching no route under `route="unmatched"`
    /// (default `true`). This adds one global middleware; disable it to keep
    /// the router's no-middleware fast path for unmatched requests.
    pub fn track_unmatched(mut self, enabled: bool) -> Self {
        self.track_unmatched = enabled;
        self
    }

    /// Include [`ProcessCollector`] metrics (default `true`).
    pub fn process_metrics(mut self, enabled: bool) -> Self {
        self.process = enabled;
        self
    }
}

impl Plugin for Metrics {
    fn install(self, app: App) -> App {
        assert!(
            self.prefix.is_empty() || is_valid_metric_name(&self.prefix),
            "invalid metrics prefix `{}`",
            self.prefix
        );
        let buckets = registry::check_buckets(&self.buckets);
        process::first_seen();
        let http = HttpMetrics::new(HttpConfig {
            prefix: self.prefix,
            buckets,
            exclude: self.exclude.iter().map(|p| PathPattern::parse(p)).collect(),
            track_unmatched: self.track_unmatched,
        });
        let handle = MetricsHandle {
            inner: Arc::new(HandleInner {
                http,
                registry: self.registry.unwrap_or_else(|| registry().clone()),
                process: self.process,
            }),
        };

        let mut app = app.state(handle.clone());
        if self.track_unmatched {
            app = app.wrap(GlobalLayer(http));
        }
        app = app.wrap_route(RouteLayer(http)).on_build(move |table| {
            http.build(table);
            Routes::new()
        });
        if self.endpoint {
            app = app.get(&self.path, move || {
                let handle = handle.clone();
                async move { handle.response() }
            });
        }
        app
    }
}

struct HandleInner {
    http: &'static HttpMetrics,
    registry: Registry,
    process: bool,
}

/// Renders an app's metrics. Registered as app state by [`Metrics`], so you
/// can serve them yourself (e.g. behind auth or on another port):
///
/// ```
/// use zapline::prelude::*;
/// use zapline::metrics::{Metrics, MetricsHandle};
///
/// async fn scrape(State(m): State<MetricsHandle>) -> Response {
///     m.into_response()
/// }
///
/// let app = App::new()
///     .plugin(Metrics::new().endpoint(false))
///     .get("/admin/metrics", scrape);
/// ```
#[derive(Clone)]
pub struct MetricsHandle {
    inner: Arc<HandleInner>,
}

impl std::fmt::Debug for MetricsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsHandle").finish_non_exhaustive()
    }
}

impl MetricsHandle {
    /// Everything in the Prometheus text format: HTTP metrics, process
    /// metrics, then the registry's metrics.
    pub fn render(&self) -> String {
        let mut buf = String::with_capacity(4096);
        let mut out = Encoder::new(&mut buf);
        self.inner.http.encode(&mut out);
        if self.inner.process {
            ProcessCollector.collect(&mut out);
        }
        self.inner.registry.encode(&mut out);
        buf
    }

    /// The registry rendered after the HTTP metrics.
    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    fn response(&self) -> Response {
        let mut res = Response::new(self.render().into());
        let h = res.headers_mut();
        h.insert(CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE_TEXT));
        h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        res
    }
}

/// The scrape response (`text/plain; version=0.0.4`).
impl IntoResponse for MetricsHandle {
    fn into_response(self) -> Response {
        self.response()
    }
}
