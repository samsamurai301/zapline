//! Health checks: liveness, readiness and a combined report, ready for
//! Kubernetes probes and load balancers.
//!
//! ```no_run
//! use std::time::Duration;
//! use zapline::prelude::*;
//! use zapline::health::{Health, HealthState};
//!
//! #[derive(Clone)]
//! struct Db;
//! impl Db {
//!     async fn ping(&self) -> Result<()> { Ok(()) }
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .state(Db)
//!         .plugin(
//!             Health::new()
//!                 .info("orders", env!("CARGO_PKG_VERSION"))
//!                 .check("db", |res: Resources| async move { res.require::<Db>()?.ping().await })
//!                 .check("cache", |_res: Resources| async move { Ok(()) })
//!                 .critical(false)
//!                 .timeout(Duration::from_secs(1)),
//!         )
//!         .on_startup(|res: Resources| async move {
//!             // Not ready until caches are warm; liveness stays up.
//!             let health = res.require::<HealthState>()?;
//!             health.set_ready(false);
//!             tokio::spawn(async move {
//!                 // ... warm caches ...
//!                 health.set_ready(true);
//!             });
//!             Ok(())
//!         })
//!         .run("0.0.0.0:8080")
//! }
//! ```
//!
//! | Endpoint | Behavior |
//! |---|---|
//! | `GET /health/live` | Always `200 {"status":"up"}`. Runs no checks. |
//! | `GET /health/ready` | Runs every check concurrently (each with a timeout): `200` when all critical checks pass (`"up"`, or `"degraded"` when a non-critical one fails), `503` otherwise or while [`HealthState`] is not ready. |
//! | `GET /health` | The readiness report plus `info` and `uptime_seconds`. |
//!
//! ```text
//! {"status":"down","checks":{"db":{"status":"up","duration_ms":1.204},
//!  "cache":{"status":"down","duration_ms":2000.113,"error":"timed out after 2s"}}}
//! ```

use std::fmt::Write as _;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};

use crate::app::{App, Plugin};
use crate::body::Body;
use crate::error::json_escape;
use crate::handler::BoxFuture;
use crate::lifecycle::Resources;
use crate::response::Response;

/// Default per-check timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// A dependency check: `Ok(())` means healthy.
///
/// Closures `Fn(Resources) -> impl Future<Output = Result<()>>` implement it,
/// so most checks are one line. Implement it on a struct for reusable checks:
///
/// ```
/// use zapline::health::HealthCheck;
/// use zapline::{Resources, Result, Error};
///
/// struct DiskSpace { min_free_bytes: u64 }
///
/// impl HealthCheck for DiskSpace {
///     async fn check(&self, _res: Resources) -> Result<()> {
///         let free = 10 * 1024 * 1024 * 1024u64; // e.g. from statvfs
///         if free < self.min_free_bytes {
///             return Err(Error::service_unavailable("low disk space"));
///         }
///         Ok(())
///     }
/// }
/// ```
pub trait HealthCheck: Send + Sync + 'static {
    /// Run the check. `res` holds the app's resources (state registered with
    /// `App::state` or in startup hooks).
    fn check(&self, res: Resources) -> impl Future<Output = crate::Result<()>> + Send;
}

impl<F, Fut> HealthCheck for F
where
    F: Fn(Resources) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = crate::Result<()>> + Send,
{
    fn check(&self, res: Resources) -> impl Future<Output = crate::Result<()>> + Send {
        self(res)
    }
}

/// Object-safe adapter for [`HealthCheck`].
trait DynCheck: Send + Sync + 'static {
    fn run(&self, res: Resources) -> BoxFuture<crate::Result<()>>;
}

struct Erased<C>(Arc<C>);

impl<C: HealthCheck> DynCheck for Erased<C> {
    fn run(&self, res: Resources) -> BoxFuture<crate::Result<()>> {
        let c = self.0.clone();
        Box::pin(async move { c.check(res).await })
    }
}

/// Readiness toggle shared with the app. Registered as state by [`Health`],
/// so it is available as `State<HealthState>` in handlers and via
/// `res.require::<HealthState>()` in startup/shutdown hooks.
///
/// Mark the app not ready during warm-up or before shutting down so load
/// balancers drain it; liveness is unaffected.
#[derive(Clone, Debug)]
pub struct HealthState {
    ready: Arc<AtomicBool>,
}

impl Default for HealthState {
    fn default() -> Self {
        HealthState {
            ready: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl HealthState {
    /// A ready state.
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

struct CheckEntry {
    name: String,
    check: Arc<dyn DynCheck>,
    critical: bool,
}

/// The health-check plugin. See the [module docs](self).
///
/// Paths can be changed or disabled (pass `""`). Checks run concurrently,
/// each bounded by [`timeout`](Self::timeout); a panicking check is reported
/// as down instead of failing the request.
#[must_use = "install it with `App::plugin`"]
pub struct Health {
    checks: Vec<CheckEntry>,
    timeout: Duration,
    live_path: String,
    ready_path: String,
    path: String,
    info: Option<(String, String)>,
    details: bool,
    cache: Option<Duration>,
    state: HealthState,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    /// `/health/live`, `/health/ready` and `/health`, no checks, 2 s timeout.
    pub fn new() -> Self {
        Health {
            checks: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            live_path: "/health/live".into(),
            ready_path: "/health/ready".into(),
            path: "/health".into(),
            info: None,
            details: true,
            cache: None,
            state: HealthState::new(),
        }
    }

    /// Add a named readiness check (critical by default).
    ///
    /// # Panics
    /// If a check with the same name was already added.
    pub fn check(mut self, name: impl Into<String>, check: impl HealthCheck) -> Self {
        let name = name.into();
        assert!(
            !self.checks.iter().any(|c| c.name == name),
            "duplicate health check `{name}`"
        );
        self.checks.push(CheckEntry {
            name,
            check: Arc::new(Erased(Arc::new(check))),
            critical: true,
        });
        self
    }

    /// Whether the most recently added check is critical. A failing
    /// non-critical check reports the app as `degraded` but keeps it ready
    /// (HTTP 200).
    ///
    /// # Panics
    /// If no check has been added yet.
    pub fn critical(mut self, critical: bool) -> Self {
        self.checks
            .last_mut()
            .expect("`critical` must be called after `check`")
            .critical = critical;
        self
    }

    /// Timeout applied to each check (default 2 s).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Liveness path (default `/health/live`; `""` disables it).
    pub fn live_path(mut self, path: impl Into<String>) -> Self {
        self.live_path = path.into();
        self
    }

    /// Readiness path (default `/health/ready`; `""` disables it).
    pub fn ready_path(mut self, path: impl Into<String>) -> Self {
        self.ready_path = path.into();
        self
    }

    /// Combined report path (default `/health`; `""` disables it).
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Static service info included in reports: `"info":{"name":..,"version":..}`.
    pub fn info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.info = Some((name.into(), version.into()));
        self
    }

    /// Include per-check results and errors in responses (default `true`).
    /// Disable on publicly reachable endpoints to only expose `status`.
    pub fn details(mut self, enabled: bool) -> Self {
        self.details = enabled;
        self
    }

    /// Reuse the last readiness result for this long, so frequent probes do
    /// not hammer dependencies (default: run checks on every request).
    pub fn cache_for(mut self, ttl: Duration) -> Self {
        self.cache = Some(ttl);
        self
    }

    /// Use an existing [`HealthState`] (e.g. one shared with a shutdown
    /// handler created before the app).
    pub fn state(mut self, state: HealthState) -> Self {
        self.state = state;
        self
    }
}

impl Plugin for Health {
    fn install(self, app: App) -> App {
        let resources: Arc<OnceLock<Resources>> = Arc::default();
        let runner = Arc::new(Runner {
            checks: self.checks,
            timeout: self.timeout,
            info: self.info,
            details: self.details,
            cache: self.cache,
            cached: Mutex::new(None),
            state: self.state.clone(),
            resources: resources.clone(),
            started: OnceLock::new(),
        });
        let r = runner.clone();
        let mut app = app
            .state(self.state)
            .on_startup(move |res: Resources| async move {
                let _ = resources.set(res);
                let _ = r.started.set(Instant::now());
                Ok(())
            });
        if !self.live_path.is_empty() {
            app = app.get(&self.live_path, || async {
                json(StatusCode::OK, r#"{"status":"up"}"#)
            });
        }
        if !self.ready_path.is_empty() {
            let r = runner.clone();
            app = app.get(&self.ready_path, move || {
                let r = r.clone();
                async move { r.respond(false).await }
            });
        }
        if !self.path.is_empty() {
            let r = runner;
            app = app.get(&self.path, move || {
                let r = r.clone();
                async move { r.respond(true).await }
            });
        }
        app
    }
}

// ---------------------------------------------------------------------------
// Running checks

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Up,
    Degraded,
    Down,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Up => "up",
            Status::Degraded => "degraded",
            Status::Down => "down",
        }
    }
}

struct CheckResult {
    name: String,
    error: Option<String>,
    duration: Duration,
}

struct Report {
    status: Status,
    /// `None` while the app is marked not ready (checks are skipped).
    checks: Option<Vec<CheckResult>>,
}

struct Runner {
    checks: Vec<CheckEntry>,
    timeout: Duration,
    info: Option<(String, String)>,
    details: bool,
    cache: Option<Duration>,
    cached: Mutex<Option<(Instant, Arc<Report>)>>,
    state: HealthState,
    resources: Arc<OnceLock<Resources>>,
    started: OnceLock<Instant>,
}

impl Runner {
    async fn respond(&self, full: bool) -> Response {
        let report = self.report().await;
        let code = match report.status {
            Status::Down => StatusCode::SERVICE_UNAVAILABLE,
            Status::Up | Status::Degraded => StatusCode::OK,
        };
        json(code, self.render(&report, full))
    }

    async fn report(&self) -> Arc<Report> {
        if !self.state.is_ready() {
            return Arc::new(Report {
                status: Status::Down,
                checks: None,
            });
        }
        if let Some(ttl) = self.cache {
            let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, r)) = cached.as_ref() {
                if at.elapsed() < ttl {
                    return r.clone();
                }
            }
        }
        let report = Arc::new(self.run().await);
        if self.cache.is_some() {
            *self.cached.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Instant::now(), report.clone()));
        }
        report
    }

    async fn run(&self) -> Report {
        let res = self.resources.get().cloned().unwrap_or_default();
        let runs = self.checks.iter().map(|c| {
            let fut = AssertUnwindSafe(c.check.run(res.clone())).catch_unwind();
            let timeout = self.timeout;
            async move {
                let start = Instant::now();
                let error = match tokio::time::timeout(timeout, fut).await {
                    Ok(Ok(Ok(()))) => None,
                    Ok(Ok(Err(e))) => Some(describe(&e)),
                    Ok(Err(_panic)) => Some("check panicked".to_owned()),
                    Err(_) => Some(format!("timed out after {timeout:?}")),
                };
                CheckResult {
                    name: c.name.clone(),
                    error,
                    duration: start.elapsed(),
                }
            }
        });
        let results = futures_util::future::join_all(runs).await;
        let mut status = Status::Up;
        for (r, c) in results.iter().zip(&self.checks) {
            if r.error.is_some() {
                if c.critical {
                    status = Status::Down;
                } else if status == Status::Up {
                    status = Status::Degraded;
                }
            }
        }
        Report {
            status,
            checks: Some(results),
        }
    }

    fn render(&self, report: &Report, full: bool) -> String {
        let mut b = String::with_capacity(128);
        b.push_str(r#"{"status":""#);
        b.push_str(report.status.as_str());
        b.push('"');
        if self.details {
            match &report.checks {
                None => b.push_str(r#","reason":"not ready""#),
                Some(checks) => {
                    b.push_str(r#","checks":{"#);
                    for (i, c) in checks.iter().enumerate() {
                        if i > 0 {
                            b.push(',');
                        }
                        b.push_str(&json_escape(&c.name));
                        let status = if c.error.is_some() { "down" } else { "up" };
                        let ms = c.duration.as_secs_f64() * 1000.0;
                        let _ = write!(b, r#":{{"status":"{status}","duration_ms":{ms:.3}"#);
                        if let Some(e) = &c.error {
                            b.push_str(r#","error":"#);
                            b.push_str(&json_escape(e));
                        }
                        b.push('}');
                    }
                    b.push('}');
                }
            }
        }
        if let Some((name, version)) = &self.info {
            let _ = write!(
                b,
                r#","info":{{"name":{},"version":{}}}"#,
                json_escape(name),
                json_escape(version)
            );
        }
        if full {
            let up = self
                .started
                .get()
                .map_or(0.0, |s| s.elapsed().as_secs_f64());
            let _ = write!(b, r#","uptime_seconds":{up:.3}"#);
        }
        b.push('}');
        b
    }
}

/// The error message, plus its underlying cause when there is one.
fn describe(e: &crate::Error) -> String {
    match e.source_error() {
        Some(src) => format!("{}: {src}", e.message()),
        None => e.message().to_owned(),
    }
}

fn json(status: StatusCode, body: impl Into<Body>) -> Response {
    let mut res = Response::new(body.into());
    *res.status_mut() = status;
    let h = res.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}
