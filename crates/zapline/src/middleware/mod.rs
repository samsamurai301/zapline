//! Middleware: `async` functions that wrap the rest of the request pipeline.
//!
//! Global middleware ([`App::wrap`](crate::App::wrap)) runs before routing;
//! route middleware ([`App::wrap_route`](crate::App::wrap_route),
//! [`Routes::wrap`](crate::Routes::wrap)) runs after it.
//!
//! ```ignore
//! App::new().wrap(|req: Request, next: Next| async move {
//!     let res = next.run(req).await;
//!     res
//! });
//! ```

use std::future::Future;
use std::sync::Arc;

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

// Built-in middleware. Each lives in its own module.
mod builtin;
pub use builtin::{Cors, DefaultHeaders, Logger, Timeout};

pub mod catch_panic;
#[cfg(feature = "compression")]
pub mod compression;
pub mod limit;
pub mod normalize_path;
pub mod rate_limit;
pub mod request_id;
pub mod security_headers;

// WP-MIDDLEWARE re-exports (owned by the middleware work package)
pub use catch_panic::CatchPanic;
#[cfg(feature = "compression")]
pub use compression::{Compression, CompressionLevel};
pub use limit::ConcurrencyLimit;
pub use normalize_path::NormalizePath;
pub use rate_limit::{
    Cidr, ClientIpHeader, RateLimit, RateLimitKey, RateLimitPolicy, TrustedProxies,
};
pub use request_id::{RequestId, SetRequestId};
pub use security_headers::SecurityHeaders;
