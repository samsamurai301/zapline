//! Structured logging and per-request tracing on top of [`tracing`].
//!
//! ```no_run
//! use zapline::prelude::*;
//! use zapline::telemetry::{self, Trace};
//!
//! fn main() -> std::io::Result<()> {
//!     telemetry::init(); // RUST_LOG-aware; pretty on a TTY, compact otherwise
//!
//!     App::new()
//!         .get("/users/{id}", || async { "ada" })
//!         .plugin(Trace::new().skip_paths(["/health/live", "/metrics"]))
//!         .run("0.0.0.0:8080")
//! }
//! ```
//!
//! Each request gets a `request` span with `http.method`, `http.route`,
//! `url.path`, `client.address`, `request_id`, `http.status_code` and
//! `latency_ms`, and one completion event: `INFO` below 400, `WARN` for
//! 4xx, `ERROR` for 5xx. Logs emitted by handlers are nested in the span.
//!
//! [`Trace`] works three ways:
//!
//! | Install | Sees | `http.route` |
//! |---|---|---|
//! | `.plugin(Trace::new())` (recommended) | every request, incl. 404s | route template, or `unmatched` |
//! | `.wrap(Trace::new())` | every request, incl. 404s | not recorded (routing happens later) |
//! | `.wrap_route(Trace::new())` | matched routes only | route template |

use std::fmt;
use std::io::IsTerminal;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use http::{HeaderName, HeaderValue};
use tracing::field::{display, Empty};
use tracing::level_filters::LevelFilter;
use tracing::{Dispatch, Span};
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub use tracing::Level;
/// The `tracing` crate, so apps can log without depending on it directly.
pub use tracing::{self, debug, error, info, trace, warn, Instrument};

use crate::app::{App, Plugin};
use crate::handler::BoxFuture;
use crate::middleware::{Middleware, Next};
use crate::request::Request;
use crate::response::Response;

/// Target of the request spans and events (filter with
/// `RUST_LOG=zapline::request=warn`).
pub const TARGET: &str = "zapline::request";

/// Env var overriding the automatic log format: `json`, `pretty`,
/// `compact` or `full`.
pub const ENV_LOG_FORMAT: &str = "ZAPLINE_LOG_FORMAT";

/// Install the default subscriber: [`Telemetry::new().init()`](Telemetry::init).
///
/// Honors `RUST_LOG` (default `info`) and [`ENV_LOG_FORMAT`]; pretty output
/// on a terminal, compact otherwise. Does nothing if a global subscriber is
/// already installed.
pub fn init() {
    Telemetry::new().init();
}

// ---------------------------------------------------------------------------
// Subscriber setup

/// Log output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// [`ENV_LOG_FORMAT`] if set, else `Pretty` on a terminal and `Compact`
    /// otherwise.
    Auto,
    /// Multi-line, human-friendly (development).
    Pretty,
    /// One line per event with span context.
    Compact,
    /// One line per event, tracing-subscriber's default layout.
    Full,
    /// Newline-delimited JSON with flattened event fields and span context
    /// (production log pipelines).
    Json,
}

/// Why [`Telemetry::try_init`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum TelemetryError {
    /// A global subscriber is already installed.
    AlreadyInitialized,
    /// The level or filter directives are invalid.
    InvalidFilter(String),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TelemetryError::AlreadyInitialized => {
                f.write_str("a global tracing subscriber is already installed")
            }
            TelemetryError::InvalidFilter(e) => write!(f, "invalid log filter: {e}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

/// Builder for the global `tracing` subscriber.
///
/// ```no_run
/// use zapline::telemetry::Telemetry;
///
/// Telemetry::new()
///     .json()
///     .level("debug")                       // used when RUST_LOG is unset
///     .with_target(false)
///     .init();
/// ```
///
/// Filter precedence: [`env_filter`](Self::env_filter) if set, else
/// `RUST_LOG` (or the variable named by [`env_var`](Self::env_var)) if
/// non-empty, else [`level`](Self::level), else `info`.
#[must_use = "call `.init()` to install the subscriber"]
pub struct Telemetry {
    format: Format,
    level: Option<String>,
    filter: Option<String>,
    env_var: String,
    target: bool,
    ansi: Option<bool>,
    thread_ids: bool,
    thread_names: bool,
    file_line: bool,
    writer: Option<BoxMakeWriter>,
    #[cfg(feature = "otlp")]
    otlp: Option<crate::otlp::OtlpConfig>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telemetry")
            .field("format", &self.format)
            .field("level", &self.level)
            .field("filter", &self.filter)
            .finish_non_exhaustive()
    }
}

impl Telemetry {
    pub fn new() -> Self {
        Telemetry {
            format: Format::Auto,
            level: None,
            filter: None,
            env_var: EnvFilter::DEFAULT_ENV.to_owned(),
            target: true,
            ansi: None,
            thread_ids: false,
            thread_names: false,
            file_line: false,
            writer: None,
            #[cfg(feature = "otlp")]
            otlp: None,
        }
    }

    /// Also send spans to an OpenTelemetry collector over OTLP/HTTP (feature
    /// `otlp`): `endpoint` is its base URL (`http://localhost:4318`) or the
    /// full `/v1/traces` URL. Without this call, setting
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` (or `..._TRACES_ENDPOINT`) does the
    /// same. `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME` and
    /// `OTEL_RESOURCE_ATTRIBUTES` are honored.
    ///
    /// ```no_run
    /// zapline::telemetry::Telemetry::new()
    ///     .otlp("http://localhost:4318")
    ///     .service_name("shop")
    ///     .init();
    /// ```
    #[cfg(feature = "otlp")]
    pub fn otlp(mut self, endpoint: impl AsRef<str>) -> Self {
        let name = self.otlp.take().and_then(|c| c.service_name);
        let mut config = crate::otlp::OtlpConfig::new(endpoint.as_ref());
        config.service_name = name;
        self.otlp = Some(config);
        self
    }

    /// The `service.name` traces are reported under (default:
    /// `OTEL_SERVICE_NAME`, else the executable's name).
    ///
    /// ```
    /// let t = zapline::telemetry::Telemetry::new().service_name("shop");
    /// ```
    #[cfg(feature = "otlp")]
    pub fn service_name(mut self, name: impl Into<String>) -> Self {
        let config = self.otlp.get_or_insert_with(|| crate::otlp::OtlpConfig {
            url: String::new(),
            headers: Vec::new(),
            service_name: None,
        });
        config.service_name = Some(name.into());
        self
    }

    pub fn format(mut self, format: Format) -> Self {
        self.format = format;
        self
    }
    /// Shorthand for `.format(Format::Json)`.
    pub fn json(self) -> Self {
        self.format(Format::Json)
    }
    /// Shorthand for `.format(Format::Pretty)`.
    pub fn pretty(self) -> Self {
        self.format(Format::Pretty)
    }
    /// Shorthand for `.format(Format::Compact)`.
    pub fn compact(self) -> Self {
        self.format(Format::Compact)
    }

    /// Default level (`"debug"`, `Level::WARN`, ...) when `RUST_LOG` is unset.
    pub fn level(mut self, level: impl fmt::Display) -> Self {
        self.level = Some(level.to_string());
        self
    }

    /// Explicit filter directives (`"info,my_app=debug,hyper=warn"`); takes
    /// precedence over `RUST_LOG`.
    pub fn env_filter(mut self, directives: impl Into<String>) -> Self {
        self.filter = Some(directives.into());
        self
    }

    /// Read filter directives from this variable instead of `RUST_LOG`.
    pub fn env_var(mut self, name: impl Into<String>) -> Self {
        self.env_var = name.into();
        self
    }

    /// Show the event target (module path). Default `true`.
    pub fn with_target(mut self, enabled: bool) -> Self {
        self.target = enabled;
        self
    }

    /// Force ANSI colors on or off (default: on for terminals unless
    /// `NO_COLOR` is set; never for JSON or custom writers).
    pub fn ansi(mut self, enabled: bool) -> Self {
        self.ansi = Some(enabled);
        self
    }

    pub fn with_thread_ids(mut self, enabled: bool) -> Self {
        self.thread_ids = enabled;
        self
    }

    pub fn with_thread_names(mut self, enabled: bool) -> Self {
        self.thread_names = enabled;
        self
    }

    /// Include the source file and line of each event.
    pub fn with_file_line(mut self, enabled: bool) -> Self {
        self.file_line = enabled;
        self
    }

    /// Write logs somewhere other than stdout, e.g. `std::io::stderr` or a
    /// non-blocking file appender.
    pub fn writer<W>(mut self, writer: W) -> Self
    where
        W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    {
        self.writer = Some(BoxMakeWriter::new(writer));
        self
    }

    /// Install as the global subscriber. Does nothing if one is already
    /// installed (e.g. by tests or another library).
    ///
    /// # Panics
    /// If the filter directives are invalid (a startup configuration error).
    pub fn init(self) {
        match self.try_init() {
            Ok(()) | Err(TelemetryError::AlreadyInitialized) => {}
            Err(e) => panic!("{e}"),
        }
    }

    /// Install as the global subscriber, also bridging `log` records.
    pub fn try_init(self) -> Result<(), TelemetryError> {
        let dispatch = self.into_dispatch()?;
        tracing::dispatcher::set_global_default(dispatch.clone())
            .map_err(|_| TelemetryError::AlreadyInitialized)?;
        // `set_default` also installs the `log` -> `tracing` bridge (ignoring
        // an existing logger); the scoped default is dropped right away.
        drop(dispatch.set_default());
        Ok(())
    }

    /// Build the subscriber without installing it, e.g. for
    /// `tracing::dispatcher::with_default` in tests.
    #[cfg_attr(not(feature = "otlp"), allow(unused_mut))]
    pub fn into_dispatch(mut self) -> Result<Dispatch, TelemetryError> {
        let filter = self.build_filter()?;
        #[cfg(feature = "otlp")]
        let otlp = {
            let configured = self.otlp.take();
            let name = configured.as_ref().and_then(|c| c.service_name.clone());
            configured
                .filter(|c| !c.url.is_empty())
                .or_else(crate::otlp::OtlpConfig::from_env)
                .map(|mut c| {
                    c.service_name = c.service_name.or(name);
                    c.layer()
                })
        };
        let layer = self.fmt_layer();
        let registry = tracing_subscriber::registry().with(filter).with(layer);
        #[cfg(feature = "otlp")]
        let registry = registry.with(otlp);
        Ok(Dispatch::new(registry))
    }

    fn build_filter(&self) -> Result<EnvFilter, TelemetryError> {
        let invalid = |e: &dyn fmt::Display| TelemetryError::InvalidFilter(e.to_string());
        if let Some(d) = &self.filter {
            return EnvFilter::builder().parse(d).map_err(|e| invalid(&e));
        }
        if let Some(env) = std::env::var(&self.env_var)
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            return Ok(EnvFilter::builder().parse_lossy(env));
        }
        let level = self.level.as_deref().unwrap_or("info");
        let level: LevelFilter = level
            .parse()
            .map_err(|_| TelemetryError::InvalidFilter(format!("unknown level `{level}`")))?;
        Ok(EnvFilter::builder().parse_lossy(level.to_string()))
    }

    fn resolved_format(&self) -> Format {
        if self.format != Format::Auto {
            return self.format;
        }
        let from_env = std::env::var(ENV_LOG_FORMAT).ok();
        match from_env.as_deref().map(str::trim) {
            Some(f) if f.eq_ignore_ascii_case("json") => Format::Json,
            Some(f) if f.eq_ignore_ascii_case("pretty") => Format::Pretty,
            Some(f) if f.eq_ignore_ascii_case("compact") => Format::Compact,
            Some(f) if f.eq_ignore_ascii_case("full") => Format::Full,
            _ if std::io::stdout().is_terminal() => Format::Pretty,
            _ => Format::Compact,
        }
    }

    fn fmt_layer<S>(self) -> Box<dyn Layer<S> + Send + Sync>
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        let format = self.resolved_format();
        let ansi = self.ansi.unwrap_or_else(|| {
            self.writer.is_none()
                && format != Format::Json
                && std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
        });
        let writer = self
            .writer
            .unwrap_or_else(|| BoxMakeWriter::new(std::io::stdout));
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(ansi && format != Format::Json)
            .with_target(self.target)
            .with_thread_ids(self.thread_ids)
            .with_thread_names(self.thread_names)
            .with_file(self.file_line)
            .with_line_number(self.file_line);
        match format {
            Format::Json => layer
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(true)
                .boxed(),
            Format::Pretty => layer.pretty().boxed(),
            Format::Compact => layer.compact().boxed(),
            Format::Full | Format::Auto => layer.boxed(),
        }
    }
}

// ---------------------------------------------------------------------------
// Request tracing

/// Response classes for [`Trace::level_for`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusClass {
    /// 1xx, 2xx and 3xx.
    Success,
    /// 4xx.
    ClientError,
    /// 5xx.
    ServerError,
}

struct TraceConfig {
    span_level: Level,
    levels: [Level; 3],
    /// Most severe level we may emit: if even that is disabled, nothing is.
    min_level: Level,
    skip: Vec<Skip>,
    request_id_header: HeaderName,
    client_ip_header: Option<HeaderName>,
    /// Set once the route recorder is installed (via `Plugin`).
    route_recorder: AtomicBool,
}

impl Clone for TraceConfig {
    fn clone(&self) -> Self {
        TraceConfig {
            span_level: self.span_level,
            levels: self.levels,
            min_level: self.min_level,
            skip: self.skip.clone(),
            request_id_header: self.request_id_header.clone(),
            client_ip_header: self.client_ip_header.clone(),
            route_recorder: AtomicBool::new(self.route_recorder.load(Ordering::Relaxed)),
        }
    }
}

#[derive(Clone)]
enum Skip {
    Exact(String),
    Prefix(String),
}

impl Skip {
    fn matches(&self, path: &str) -> bool {
        match self {
            Skip::Exact(p) => p == path,
            Skip::Prefix(p) => path.starts_with(p.as_str()),
        }
    }
}

/// Request tracing middleware: one span and one completion event per
/// request. See the [module docs](self) for where to install it.
///
/// ```
/// use zapline::prelude::*;
/// use zapline::telemetry::{Level, StatusClass, Trace};
///
/// let app = App::new()
///     .get("/", || async { "hi" })
///     .plugin(
///         Trace::new()
///             .level(Level::DEBUG)                          // span + success events
///             .level_for(StatusClass::ClientError, Level::INFO)
///             .skip_paths(["/health/live", "/metrics", "/static/*"]),
///     );
/// ```
///
/// When the subscriber filters out every level `Trace` could emit, it adds no
/// work beyond one level comparison per request.
#[derive(Clone)]
pub struct Trace {
    cfg: Arc<TraceConfig>,
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Trace")
            .field("span_level", &self.cfg.span_level)
            .field("levels", &self.cfg.levels)
            .finish_non_exhaustive()
    }
}

impl Trace {
    /// `INFO` spans; `INFO` / `WARN` / `ERROR` completion events for
    /// success / 4xx / 5xx; request ids from `x-request-id`.
    pub fn new() -> Self {
        Trace {
            cfg: Arc::new(TraceConfig {
                span_level: Level::INFO,
                levels: [Level::INFO, Level::WARN, Level::ERROR],
                min_level: Level::ERROR,
                skip: Vec::new(),
                request_id_header: HeaderName::from_static("x-request-id"),
                client_ip_header: None,
                route_recorder: AtomicBool::new(false),
            }),
        }
    }

    fn update(mut self, f: impl FnOnce(&mut TraceConfig)) -> Self {
        let cfg = Arc::make_mut(&mut self.cfg);
        f(cfg);
        cfg.min_level = cfg.levels.iter().copied().fold(cfg.span_level, Level::min);
        self
    }

    /// Level of the request span and of success events (default `INFO`).
    pub fn level(self, level: Level) -> Self {
        self.update(|c| {
            c.span_level = level;
            c.levels[0] = level;
        })
    }

    /// Level of the completion event for a class of responses.
    pub fn level_for(self, class: StatusClass, level: Level) -> Self {
        self.update(|c| c.levels[class as usize] = level)
    }

    /// Request paths (or route templates) to skip entirely, e.g. probes and
    /// scrapes. A trailing `*` matches a prefix: `"/static/*"`.
    pub fn skip_paths<I, S>(self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.update(|c| {
            c.skip.extend(paths.into_iter().map(|p| {
                let p: String = p.into();
                match p.strip_suffix('*') {
                    Some(prefix) => Skip::Prefix(prefix.to_owned()),
                    None => Skip::Exact(p),
                }
            }))
        })
    }

    /// Header carrying the request id (default `x-request-id`). Read from the
    /// request, or else from the response (as set by a request-id middleware).
    ///
    /// # Panics
    /// If `name` is not a valid header name.
    pub fn request_id_header(self, name: &str) -> Self {
        let name = HeaderName::try_from(name).expect("invalid request id header name");
        self.update(|c| c.request_id_header = name)
    }

    /// Take `client.address` from the first entry of this header (e.g.
    /// `x-forwarded-for`) instead of the peer address. Only enable it behind
    /// a proxy that sets the header, since clients can forge it.
    ///
    /// # Panics
    /// If `name` is not a valid header name.
    pub fn client_ip_header(self, name: &str) -> Self {
        let name = HeaderName::try_from(name).expect("invalid client IP header name");
        self.update(|c| c.client_ip_header = Some(name))
    }
}

/// `.plugin(Trace::new())`: global tracing (sees 404s) plus a tiny route
/// middleware that records the matched route template into the span.
impl Plugin for Trace {
    fn install(self, app: App) -> App {
        self.cfg.route_recorder.store(true, Ordering::Relaxed);
        app.wrap(self).wrap_route(RecordRoute)
    }
}

/// Shared between the global `Trace` middleware and [`RecordRoute`].
#[derive(Clone)]
struct RouteCell(Arc<RouteCellInner>);

struct RouteCellInner {
    span: Span,
    route: OnceLock<&'static str>,
}

/// Records the matched route into the enclosing request span.
struct RecordRoute;

impl Middleware for RecordRoute {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        if let (Some(cell), Some(route)) = (req.extensions().get::<RouteCell>(), req.matched_path())
        {
            if cell.0.route.set(route).is_ok() {
                cell.0.span.record("http.route", display(route));
            }
        }
        next.run(req)
    }
}

macro_rules! dyn_level {
    ($level:expr, $mac:ident!(parent: $parent:expr, $($args:tt)*)) => {{
        let level = $level;
        if level == Level::ERROR {
            tracing::$mac!(target: TARGET, parent: $parent, Level::ERROR, $($args)*)
        } else if level == Level::WARN {
            tracing::$mac!(target: TARGET, parent: $parent, Level::WARN, $($args)*)
        } else if level == Level::INFO {
            tracing::$mac!(target: TARGET, parent: $parent, Level::INFO, $($args)*)
        } else if level == Level::DEBUG {
            tracing::$mac!(target: TARGET, parent: $parent, Level::DEBUG, $($args)*)
        } else {
            tracing::$mac!(target: TARGET, parent: $parent, Level::TRACE, $($args)*)
        }
    }};
    ($level:expr, $mac:ident!($($args:tt)*)) => {{
        let level = $level;
        if level == Level::ERROR {
            tracing::$mac!(target: TARGET, Level::ERROR, $($args)*)
        } else if level == Level::WARN {
            tracing::$mac!(target: TARGET, Level::WARN, $($args)*)
        } else if level == Level::INFO {
            tracing::$mac!(target: TARGET, Level::INFO, $($args)*)
        } else if level == Level::DEBUG {
            tracing::$mac!(target: TARGET, Level::DEBUG, $($args)*)
        } else {
            tracing::$mac!(target: TARGET, Level::TRACE, $($args)*)
        }
    }};
}

/// Request data needed by the completion event when the span is disabled
/// (e.g. `RUST_LOG=warn`): the event then carries the request fields itself.
/// Refcounted clones: no strings are copied on the filtered-out path.
struct Fields {
    method: http::Method,
    uri: http::Uri,
    client: Option<IpAddr>,
    request_id: Option<HeaderValue>,
}

impl TraceConfig {
    fn skipped(&self, req: &Request) -> bool {
        !self.skip.is_empty()
            && self
                .skip
                .iter()
                .any(|s| s.matches(req.path()) || req.matched_path().is_some_and(|r| s.matches(r)))
    }

    fn client(&self, req: &Request) -> Option<IpAddr> {
        let forwarded = self.client_ip_header.as_ref().and_then(|h| {
            let v = req.headers().get(h)?.to_str().ok()?;
            v.split(',').next()?.trim().parse().ok()
        });
        forwarded.or_else(|| req.remote_addr().map(|a| a.ip()))
    }

    fn level_for(&self, status: u16) -> Level {
        match status {
            500.. => self.levels[2],
            400..=499 => self.levels[1],
            _ => self.levels[0],
        }
    }
}

/// A loggable request id: visible ASCII, at most 200 bytes.
fn valid_id(v: &HeaderValue) -> Option<&str> {
    v.to_str().ok().filter(|v| !v.is_empty() && v.len() <= 200)
}

fn header_id<'a>(headers: &'a http::HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(valid_id)
}

impl Middleware for Trace {
    fn handle(&self, mut req: Request, next: Next) -> BoxFuture<Response> {
        let cfg = &*self.cfg;
        if !tracing::level_enabled!(cfg.min_level) || cfg.skipped(&req) {
            return next.run(req);
        }
        let start = Instant::now();
        let route = req.matched_path();
        let client = cfg.client(&req);
        let request_id = header_id(req.headers(), &cfg.request_id_header);
        let span = dyn_level!(
            cfg.span_level,
            span!(
                "request",
                http.method = %req.method(),
                http.route = route.map(display),
                url.path = %req.path(),
                client.address = client.map(display),
                request_id = request_id.map(display),
                traceparent = req.headers().get("traceparent").and_then(|v| v.to_str().ok()).map(display),
                http.status_code = Empty,
                latency_ms = Empty,
            )
        );
        let has_request_id = request_id.is_some();
        let fields = span.is_disabled().then(|| Fields {
            method: req.method().clone(),
            uri: req.uri().clone(),
            client,
            request_id: req.headers().get(&cfg.request_id_header).cloned(),
        });
        // Learn the route from `RecordRoute` (plugin mode). Only worth the
        // allocation while the span is recording.
        let track_route =
            fields.is_none() && route.is_none() && cfg.route_recorder.load(Ordering::Relaxed);
        let cell = track_route.then(|| {
            let cell = RouteCell(Arc::new(RouteCellInner {
                span: span.clone(),
                route: OnceLock::new(),
            }));
            req.extensions_mut().insert(cell.clone());
            cell
        });
        let cfg = self.cfg.clone();
        let fut = next.run(req).instrument(span.clone());
        Box::pin(async move {
            let res = fut.await;
            let status = res.status().as_u16();
            // Microsecond precision keeps the logged number short.
            let latency_ms = start.elapsed().as_micros() as f64 / 1000.0;
            // With the route recorder installed, no recorded route on a
            // 404/405 means the router matched nothing.
            let unmatched = cell.as_ref().is_some_and(|c| c.0.route.get().is_none())
                && (status == 404 || status == 405);
            let response_id = (!has_request_id)
                .then(|| header_id(res.headers(), &cfg.request_id_header))
                .flatten();
            let level = cfg.level_for(status);
            match fields {
                None => {
                    if unmatched {
                        span.record("http.route", display("unmatched"));
                    }
                    if let Some(id) = response_id {
                        span.record("request_id", display(id));
                    }
                    dyn_level!(
                        level,
                        event!(
                            parent: &span,
                            http.status_code = status,
                            latency_ms,
                            "request completed"
                        )
                    );
                    // Recorded after the event so text logs don't repeat them;
                    // span consumers (OpenTelemetry, span close) still see them.
                    span.record("http.status_code", status);
                    span.record("latency_ms", latency_ms);
                }
                Some(f) => {
                    let request_id = f.request_id.as_ref().and_then(valid_id).or(response_id);
                    dyn_level!(
                        level,
                        event!(
                            http.method = %f.method,
                            http.route = route.map(display),
                            url.path = %f.uri.path(),
                            client.address = f.client.map(display),
                            request_id = request_id.map(display),
                            http.status_code = status,
                            latency_ms,
                            "request completed"
                        )
                    );
                }
            }
            res
        })
    }
}
