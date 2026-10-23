//! Turn handler panics into `500` responses instead of dropped connections.
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::middleware::CatchPanic;
//!
//! let app = App::new()
//!     .wrap(CatchPanic::new())
//!     .get("/boom", || async { if true { panic!("oops") } "unreachable" });
//! ```
//!
//! Panics can only be caught when the binary unwinds: a build with
//! `panic = "abort"` (as in this workspace's `release` profile) terminates
//! the process instead. Rust's panic hook still prints the panic to stderr.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use futures_util::FutureExt;
use http::{Method, Uri};

use super::{Middleware, Next};
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::{IntoResponse, Response};

type PanicHandler = Arc<dyn Fn(String) -> Response + Send + Sync>;

/// Catches panics in downstream middleware and handlers and responds with
/// `500 {"error":"internal server error","code":"panic"}`.
///
/// Install it first (outermost) so it covers everything after it. The panic
/// message is logged (via `tracing` when that feature is on), never sent to
/// the client. Customize the response with [`CatchPanic::handler`]:
///
/// ```
/// use zapline::prelude::*;
/// use zapline::middleware::CatchPanic;
///
/// let mw = CatchPanic::new().handler(|_msg: String| {
///     (StatusCode::INTERNAL_SERVER_ERROR, "something broke").into_response()
/// });
/// ```
#[derive(Clone, Default)]
pub struct CatchPanic {
    handler: Option<PanicHandler>,
}

impl CatchPanic {
    /// Respond to panics with the default `500` JSON error.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the response yourself from the panic message.
    pub fn handler<F, R>(mut self, f: F) -> Self
    where
        F: Fn(String) -> R + Send + Sync + 'static,
        R: IntoResponse,
    {
        self.handler = Some(Arc::new(move |msg| f(msg).into_response()));
        self
    }
}

impl Middleware for CatchPanic {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let ctx = PanicContext {
            method: req.method().clone(),
            uri: req.uri().clone(),
            handler: self.handler.clone(),
        };
        // Middleware and handlers may panic while building their future, too.
        let fut = match catch_unwind(AssertUnwindSafe(|| next.run(req))) {
            Ok(fut) => fut,
            Err(payload) => return Box::pin(std::future::ready(ctx.respond(payload))),
        };
        Box::pin(async move {
            match AssertUnwindSafe(fut).catch_unwind().await {
                Ok(res) => res,
                Err(payload) => ctx.respond(payload),
            }
        })
    }
}

/// What a panic response and log line need from the request.
struct PanicContext {
    method: Method,
    uri: Uri,
    handler: Option<PanicHandler>,
}

impl PanicContext {
    fn respond(self, payload: Box<dyn Any + Send>) -> Response {
        let msg = panic_message(payload.as_ref());
        log_panic_str(self.method.as_str(), self.uri.path(), &msg);
        // With dev errors on, the default response carries the panic (and
        // where it happened) for the dev error page.
        let location = crate::dev::take_panic_location();
        match self.handler {
            // A panicking custom handler must not escape either.
            Some(h) => catch_unwind(AssertUnwindSafe(|| h(msg)))
                .unwrap_or_else(|_| default_response(String::new(), None)),
            None => default_response(msg, location),
        }
    }
}

fn default_response(msg: String, location: Option<String>) -> Response {
    crate::dev::panic_response(msg, location)
}

pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[cfg(feature = "tracing")]
pub(crate) fn log_panic_str(method: &str, path: &str, msg: &str) {
    tracing::error!(method, path, panic = %msg, "request handler panicked");
}

#[cfg(not(feature = "tracing"))]
pub(crate) fn log_panic_str(method: &str, path: &str, msg: &str) {
    eprintln!("[zapline] {method} {path}: request handler panicked: {msg}");
}
