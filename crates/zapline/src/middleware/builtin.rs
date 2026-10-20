//! Logger, Timeout, Cors and DefaultHeaders.

use std::sync::Arc;
use std::time::{Duration, Instant};

use http::header::{self, HeaderName, HeaderValue};
use http::{Method, StatusCode};

use super::request_id::{RequestId, X_REQUEST_ID};
use super::{Middleware, Next};
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::{IntoResponse, Response};

/// Logs `METHOD path -> status (latency)` to stderr, plus the matched
/// route (when used as route middleware) and the request ID (when
/// [`SetRequestId`](super::SetRequestId) is installed).
///
/// ```
/// use zapline::prelude::*;
/// use zapline::middleware::Logger;
///
/// // Global: logs every request, including 404s.
/// let app = App::new().wrap(Logger);
/// // Route middleware: also logs the route pattern, e.g. `/users/{id}`.
/// let app = App::new().get("/users/{id}", || async { "user" }).wrap_route(Logger);
/// ```
#[derive(Clone, Copy, Default)]
pub struct Logger;

impl Middleware for Logger {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let start = Instant::now();
        let method = req.method().clone();
        let uri = req.uri().clone();
        let route = req.matched_path();
        let id = req.extensions().get::<RequestId>().cloned();
        let fut = next.run(req);
        Box::pin(async move {
            let res = fut.await;
            let elapsed = start.elapsed();
            let id = id.or_else(|| {
                res.headers()
                    .get(X_REQUEST_ID)
                    .and_then(|v| RequestId::parse(v.to_str().ok()?))
            });
            let route = route.filter(|r| *r != uri.path());
            eprintln!(
                "{method} {} -> {} ({elapsed:?}){}{}",
                uri.path(),
                res.status().as_u16(),
                Field("route", route),
                Field("id", id),
            );
            res
        })
    }
}

/// Displays ` name=value`, or nothing for `None`.
struct Field<T>(&'static str, Option<T>);

impl<T: std::fmt::Display> std::fmt::Display for Field<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.1 {
            Some(v) => write!(f, " {}={v}", self.0),
            None => Ok(()),
        }
    }
}

/// Fails requests that take longer than the given duration with
/// `503 {"error":"request timed out","code":"timeout"}`.
///
/// ```
/// use std::time::Duration;
/// use zapline::middleware::Timeout;
///
/// let mw = Timeout::new(Duration::from_secs(10));
/// ```
#[derive(Clone, Copy)]
pub struct Timeout(pub Duration);

impl Timeout {
    /// Time out requests after `duration`.
    pub fn new(duration: Duration) -> Self {
        Timeout(duration)
    }
}

impl Middleware for Timeout {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let dur = self.0;
        let fut = next.run(req);
        Box::pin(async move {
            match tokio::time::timeout(dur, fut).await {
                Ok(res) => res,
                Err(_) => crate::Error::new(StatusCode::SERVICE_UNAVAILABLE, "request timed out")
                    .with_code("timeout")
                    .into_response(),
            }
        })
    }
}

type OriginFn = Arc<dyn Fn(&HeaderValue) -> bool + Send + Sync>;

const STAR: HeaderValue = HeaderValue::from_static("*");

/// Cross-Origin Resource Sharing.
///
/// Install it globally so preflight (`OPTIONS`) requests are answered
/// before routing.
///
/// ```
/// use std::time::Duration;
/// use zapline::prelude::*;
/// use zapline::middleware::Cors;
///
/// let cors = Cors::origins(["https://app.example.com"])
///     .allow_origin_fn(|origin: &HeaderValue| {
///         origin.as_bytes().ends_with(b".preview.example.com")
///     })
///     .allow_credentials()
///     .expose_headers("x-request-id, ratelimit-remaining")
///     .max_age(Duration::from_secs(600));
/// let app = App::new().wrap(cors);
/// ```
#[derive(Clone)]
pub struct Cors {
    origins: Option<Arc<[HeaderValue]>>,
    origin_fn: Option<OriginFn>,
    methods: HeaderValue,
    headers: HeaderValue,
    expose: Option<HeaderValue>,
    credentials: bool,
    max_age: Option<HeaderValue>,
}

impl Cors {
    /// Allow any origin, common methods and any request header.
    pub fn permissive() -> Self {
        Cors {
            origins: None,
            origin_fn: None,
            methods: HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"),
            headers: STAR,
            expose: None,
            credentials: false,
            max_age: Some(HeaderValue::from_static("86400")),
        }
    }

    /// Allow only the listed origins (e.g. `"https://example.com"`).
    ///
    /// # Panics
    /// If an origin is not a valid header value.
    pub fn origins<I, S>(origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let list = origins
            .into_iter()
            .map(|o| HeaderValue::from_str(o.as_ref()).expect("invalid CORS origin"))
            .collect();
        Cors {
            origins: Some(list),
            ..Self::permissive()
        }
    }

    /// Also allow origins for which `f` returns `true` (in addition to any
    /// listed with [`Cors::origins`]).
    pub fn allow_origin_fn<F>(mut self, f: F) -> Self
    where
        F: Fn(&HeaderValue) -> bool + Send + Sync + 'static,
    {
        self.origin_fn = Some(Arc::new(f));
        self
    }

    /// `Access-Control-Allow-Methods` for preflights.
    pub fn allow_methods(mut self, methods: &str) -> Self {
        self.methods = HeaderValue::from_str(methods).expect("invalid CORS methods");
        self
    }

    /// `Access-Control-Allow-Headers` for preflights (`*` = any).
    pub fn allow_headers(mut self, headers: &str) -> Self {
        self.headers = HeaderValue::from_str(headers).expect("invalid CORS headers");
        self
    }

    /// Allow cookies and auth headers. With any-origin configs the request
    /// origin is echoed instead of `*`, and `*` method/header lists echo the
    /// request, as the spec requires.
    pub fn allow_credentials(mut self) -> Self {
        self.credentials = true;
        self
    }

    /// `Access-Control-Expose-Headers`: response headers scripts may read.
    pub fn expose_headers(mut self, headers: &str) -> Self {
        self.expose = Some(HeaderValue::from_str(headers).expect("invalid CORS expose headers"));
        self
    }

    /// How long browsers may cache a preflight (default 24 h).
    pub fn max_age(mut self, age: Duration) -> Self {
        self.max_age = Some(HeaderValue::from(age.as_secs()));
        self
    }

    fn any_origin(&self) -> bool {
        self.origins.is_none() && self.origin_fn.is_none()
    }

    /// Whether responses depend on the request's `Origin`.
    fn varies(&self) -> bool {
        !self.any_origin() || self.credentials
    }

    fn allowed_origin(&self, origin: &HeaderValue) -> Option<HeaderValue> {
        if self.any_origin() {
            return Some(if self.credentials {
                origin.clone()
            } else {
                STAR
            });
        }
        let listed = self.origins.as_ref().is_some_and(|l| l.contains(origin));
        (listed || self.origin_fn.as_ref().is_some_and(|f| f(origin))).then(|| origin.clone())
    }

    /// `*` is not honored with credentials, so echo the request instead.
    fn list_or_echo(&self, list: &HeaderValue, req: &Request, name: HeaderName) -> HeaderValue {
        if self.credentials && list == STAR {
            if let Some(v) = req.headers().get(name) {
                return v.clone();
            }
        }
        list.clone()
    }

    fn preflight(&self, req: &Request) -> Response {
        let mut res = StatusCode::NO_CONTENT.into_response();
        let allowed = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|o| self.allowed_origin(o));
        let methods = self.list_or_echo(&self.methods, req, header::ACCESS_CONTROL_REQUEST_METHOD);
        let headers = self.list_or_echo(&self.headers, req, header::ACCESS_CONTROL_REQUEST_HEADERS);
        let h = res.headers_mut();
        h.insert(
            header::VARY,
            HeaderValue::from_static(
                "origin, access-control-request-method, access-control-request-headers",
            ),
        );
        if let Some(origin) = allowed {
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
            if self.credentials {
                h.insert(
                    header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                    HeaderValue::from_static("true"),
                );
            }
            h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, methods);
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, headers);
            if let Some(age) = &self.max_age {
                h.insert(header::ACCESS_CONTROL_MAX_AGE, age.clone());
            }
        }
        res
    }
}

impl Middleware for Cors {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let is_preflight = req.method() == Method::OPTIONS
            && req
                .headers()
                .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
        if is_preflight {
            return Box::pin(std::future::ready(self.preflight(&req)));
        }
        let allowed = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|o| self.allowed_origin(o));
        let vary = self.varies();
        if allowed.is_none() && !vary {
            return next.run(req);
        }
        let credentials = self.credentials;
        let expose = self.expose.clone();
        let fut = next.run(req);
        Box::pin(async move {
            let mut res = fut.await;
            let h = res.headers_mut();
            if let Some(origin) = allowed {
                h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
                if credentials {
                    h.insert(
                        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                        HeaderValue::from_static("true"),
                    );
                }
                if let Some(expose) = expose {
                    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, expose);
                }
            }
            if vary {
                h.append(header::VARY, HeaderValue::from_static("origin"));
            }
            res
        })
    }
}

/// Adds a fixed set of headers to every response (headers the handler set
/// win).
///
/// ```
/// use zapline::middleware::DefaultHeaders;
/// use zapline::{header, HeaderValue};
///
/// let mw = DefaultHeaders::new().add(header::SERVER, HeaderValue::from_static("zapline"));
/// ```
#[derive(Clone, Default)]
pub struct DefaultHeaders(Arc<Vec<(HeaderName, HeaderValue)>>);

impl DefaultHeaders {
    /// No headers yet; add them with [`add`](Self::add).
    pub fn new() -> Self {
        Self::default()
    }
    /// Add a header (sent unless the response already has it).
    pub fn add(mut self, name: HeaderName, value: HeaderValue) -> Self {
        Arc::make_mut(&mut self.0).push((name, value));
        self
    }
}

impl Middleware for DefaultHeaders {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let headers = self.0.clone();
        let fut = next.run(req);
        Box::pin(async move {
            let mut res = fut.await;
            let h = res.headers_mut();
            for (k, v) in headers.iter() {
                if !h.contains_key(k) {
                    h.insert(k.clone(), v.clone());
                }
            }
            res
        })
    }
}
