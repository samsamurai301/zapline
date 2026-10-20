//! Load shedding: cap the number of requests processed at once.
//!
//! ```
//! use std::time::Duration;
//! use zapline::prelude::*;
//! use zapline::middleware::ConcurrencyLimit;
//!
//! // At most 512 requests in flight; wait up to 50 ms for a slot, then 503.
//! let app = App::new().wrap(
//!     ConcurrencyLimit::new(512).queue_timeout(Duration::from_millis(50)),
//! );
//! ```

use std::sync::Arc;
use std::time::Duration;

use http::header::{HeaderValue, RETRY_AFTER};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{Middleware, Next};
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::Error;

/// Limits how many requests run concurrently; excess requests get
/// `503 Service Unavailable` with `Retry-After` and code `"overloaded"`.
///
/// By default excess requests are shed immediately; set
/// [`queue_timeout`](Self::queue_timeout) to let them wait briefly for a
/// slot. A slot is held until the response head is produced (streaming
/// bodies are not counted). Clones share the same limit, and so do all
/// routes under one [`Routes::wrap`](crate::Routes::wrap), so you can give
/// an expensive group its own budget.
#[derive(Clone)]
pub struct ConcurrencyLimit {
    semaphore: Arc<Semaphore>,
    max: usize,
    queue_timeout: Duration,
    retry_after: HeaderValue,
}

impl ConcurrencyLimit {
    /// Allow at most `max` requests in flight.
    ///
    /// # Panics
    /// If `max` is 0 or larger than [`Semaphore::MAX_PERMITS`].
    pub fn new(max: usize) -> Self {
        assert!(
            max > 0 && max <= Semaphore::MAX_PERMITS,
            "ConcurrencyLimit: max must be between 1 and {}",
            Semaphore::MAX_PERMITS
        );
        ConcurrencyLimit {
            semaphore: Arc::new(Semaphore::new(max)),
            max,
            queue_timeout: Duration::ZERO,
            retry_after: HeaderValue::from_static("1"),
        }
    }

    /// How long a request may wait for a free slot (default: 0, shed
    /// immediately).
    pub fn queue_timeout(mut self, timeout: Duration) -> Self {
        self.queue_timeout = timeout;
        self
    }

    /// The `Retry-After` value sent with `503`, in seconds (default 1).
    pub fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after = HeaderValue::from(secs);
        self
    }

    /// The configured maximum.
    pub fn max(&self) -> usize {
        self.max
    }

    /// Slots currently free.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    fn overloaded(retry_after: HeaderValue) -> Response {
        Error::service_unavailable("server is overloaded, try again later")
            .with_code("overloaded")
            .with_header(RETRY_AFTER, retry_after)
            .into_response()
    }
}

impl Middleware for ConcurrencyLimit {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        if let Ok(permit) = self.semaphore.clone().try_acquire_owned() {
            return with_permit(permit, next.run(req));
        }
        if self.queue_timeout.is_zero() {
            let res = Self::overloaded(self.retry_after.clone());
            return Box::pin(std::future::ready(res));
        }
        let semaphore = self.semaphore.clone();
        let timeout = self.queue_timeout;
        let retry_after = self.retry_after.clone();
        Box::pin(async move {
            match tokio::time::timeout(timeout, semaphore.acquire_owned()).await {
                Ok(Ok(_permit)) => next.run(req).await,
                _ => Self::overloaded(retry_after),
            }
        })
    }
}

fn with_permit(permit: OwnedSemaphorePermit, fut: BoxFuture<Response>) -> BoxFuture<Response> {
    Box::pin(async move {
        let res = fut.await;
        drop(permit);
        res
    })
}
