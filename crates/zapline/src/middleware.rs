//! Middleware: `async` functions that wrap the rest of the request pipeline.
//!
//! ```ignore
//! App::new().wrap(|req: Request, next: Next| async move {
//!     let res = next.run(req).await;
//!     res
//! });
//! ```

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::header::{self, HeaderName, HeaderValue};
use http::{Method, StatusCode};

use crate::handler::{BoxEndpoint, BoxFuture, Endpoint};
use crate::request::Request;
use crate::response::{IntoResponse, Response};

pub trait Middleware: Send + Sync + 'static {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response>;
}

impl<F, Fut, R> Middleware for F
where
    F: Fn(Request, Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoResponse,
{
    #[inline]
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let fut = self(req, next);
        Box::pin(async move { fut.await.into_response() })
    }
}

/// The remainder of the pipeline after the current middleware.
pub struct Next {
    chain: Arc<[Arc<dyn Middleware>]>,
    idx: usize,
    endpoint: BoxEndpoint,
}

impl Next {
    pub(crate) fn new(chain: Arc<[Arc<dyn Middleware>]>, endpoint: BoxEndpoint) -> Self {
        Next {
            chain,
            idx: 0,
            endpoint,
        }
    }

    #[inline]
    pub fn run(self, req: Request) -> BoxFuture<Response> {
        match self.chain.get(self.idx) {
            Some(mw) => {
                let mw = mw.clone();
                mw.handle(
                    req,
                    Next {
                        idx: self.idx + 1,
                        ..self
                    },
                )
            }
            None => self.endpoint.call(req),
        }
    }
}

/// An endpoint wrapped in a middleware stack (used for route groups).
pub(crate) struct Wrapped {
    pub(crate) chain: Arc<[Arc<dyn Middleware>]>,
    pub(crate) endpoint: BoxEndpoint,
}

impl Endpoint for Wrapped {
    #[inline]
    fn call(&self, req: Request) -> BoxFuture<Response> {
        Next::new(self.chain.clone(), self.endpoint.clone()).run(req)
    }
}

// ---------------------------------------------------------------------------
// Built-ins

/// Logs `METHOD path -> status (latency)` to stderr.
#[derive(Clone, Copy, Default)]
pub struct Logger;

impl Middleware for Logger {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let start = Instant::now();
        let method = req.method().clone();
        let path = req.path().to_owned();
        Box::pin(async move {
            let res = next.run(req).await;
            eprintln!(
                "{method} {path} -> {} ({:?})",
                res.status().as_u16(),
                start.elapsed()
            );
            res
        })
    }
}

/// Fails requests that take longer than the given duration with `503`.
#[derive(Clone, Copy)]
pub struct Timeout(pub Duration);

impl Middleware for Timeout {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let dur = self.0;
        Box::pin(async move {
            match tokio::time::timeout(dur, next.run(req)).await {
                Ok(res) => res,
                Err(_) => crate::Error::new(StatusCode::SERVICE_UNAVAILABLE, "request timed out")
                    .into_response(),
            }
        })
    }
}

/// Cross-Origin Resource Sharing.
#[derive(Clone)]
pub struct Cors {
    origins: Option<Vec<HeaderValue>>,
    methods: HeaderValue,
    headers: HeaderValue,
    credentials: bool,
    max_age: Option<HeaderValue>,
}

impl Cors {
    /// Allow any origin, common methods and any request header.
    pub fn permissive() -> Self {
        Cors {
            origins: None,
            methods: HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"),
            headers: HeaderValue::from_static("*"),
            credentials: false,
            max_age: Some(HeaderValue::from_static("86400")),
        }
    }

    /// Allow only the listed origins (e.g. `"https://example.com"`).
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

    pub fn allow_methods(mut self, methods: &str) -> Self {
        self.methods = HeaderValue::from_str(methods).expect("invalid CORS methods");
        self
    }
    pub fn allow_headers(mut self, headers: &str) -> Self {
        self.headers = HeaderValue::from_str(headers).expect("invalid CORS headers");
        self
    }
    pub fn allow_credentials(mut self) -> Self {
        self.credentials = true;
        self
    }

    fn allowed_origin(&self, req: &Request) -> Option<HeaderValue> {
        let origin = req.headers().get(header::ORIGIN)?;
        match &self.origins {
            None if self.credentials => Some(origin.clone()),
            None => Some(HeaderValue::from_static("*")),
            Some(list) => list.iter().find(|o| *o == origin).cloned(),
        }
    }

    fn apply(&self, res: &mut Response, origin: HeaderValue) {
        let h = res.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        if self.credentials {
            h.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
        if self.origins.is_some() || self.credentials {
            h.append(header::VARY, HeaderValue::from_static("origin"));
        }
    }
}

impl Middleware for Cors {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let origin = self.allowed_origin(&req);
        let preflight = req.method() == Method::OPTIONS
            && req
                .headers()
                .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
        let this = self.clone();
        Box::pin(async move {
            if preflight {
                let mut res = StatusCode::NO_CONTENT.into_response();
                if let Some(o) = origin {
                    this.apply(&mut res, o);
                    let h = res.headers_mut();
                    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, this.methods.clone());
                    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, this.headers.clone());
                    if let Some(age) = &this.max_age {
                        h.insert(header::ACCESS_CONTROL_MAX_AGE, age.clone());
                    }
                }
                return res;
            }
            let mut res = next.run(req).await;
            if let Some(o) = origin {
                this.apply(&mut res, o);
            }
            res
        })
    }
}

/// Adds a fixed set of headers to every response.
#[derive(Clone, Default)]
pub struct DefaultHeaders(Vec<(HeaderName, HeaderValue)>);

impl DefaultHeaders {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.0.push((name, value));
        self
    }
}

impl Middleware for DefaultHeaders {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let headers = self.0.clone();
        Box::pin(async move {
            let mut res = next.run(req).await;
            let h = res.headers_mut();
            for (k, v) in headers {
                h.entry(k).or_insert(v);
            }
            res
        })
    }
}
