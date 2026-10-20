//! Request IDs: tag every request with a unique, log-friendly identifier.
//!
//! [`SetRequestId`] reuses a valid incoming `x-request-id` (so IDs survive a
//! hop through a proxy or another service) or generates a new UUIDv7-style
//! one. The ID is stored in the request extensions, written to the request
//! headers for downstream code, and echoed in the response header.
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::middleware::{RequestId, SetRequestId};
//!
//! let app = App::new()
//!     .wrap(SetRequestId::new())
//!     .get("/", |id: RequestId| async move { format!("request {id}") });
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use http::header::{HeaderName, HeaderValue};

use super::{Middleware, Next};
use crate::extract::FromRequestParts;
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::Response;
use crate::Error;

/// The default request ID header.
pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Longest incoming ID that is accepted as-is.
const MAX_LEN: usize = 128;

/// A request's unique identifier.
///
/// Cheap to clone. Use it as an extractor: when [`SetRequestId`] is not
/// installed it falls back to a valid `x-request-id` header, or a fresh ID
/// (stable for the rest of the request).
///
/// ```
/// use zapline::middleware::RequestId;
///
/// async fn handler(id: RequestId) -> String {
///     format!("your request id is {id}")
/// }
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RequestId(HeaderValue);

impl RequestId {
    /// A fresh UUIDv7-formatted ID (time-ordered, 74 random bits).
    pub fn generate() -> Self {
        RequestId(uuid_v7())
    }

    /// Parse an ID, accepting at most 128 characters of `[A-Za-z0-9._:-]`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::from_header(&HeaderValue::from_str(s).ok()?)
    }

    fn from_header(v: &HeaderValue) -> Option<Self> {
        is_valid(v.as_bytes()).then(|| RequestId(v.clone()))
    }

    /// The ID as a string slice.
    pub fn as_str(&self) -> &str {
        // Only ever built from validated ASCII.
        self.0.to_str().unwrap_or_default()
    }

    /// The ID as a header value.
    pub fn header_value(&self) -> &HeaderValue {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RequestId").field(&self.as_str()).finish()
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromRequestParts for RequestId {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        if let Some(id) = req.extensions().get::<RequestId>() {
            return Ok(id.clone());
        }
        let id = req
            .headers()
            .get(X_REQUEST_ID)
            .and_then(RequestId::from_header)
            .unwrap_or_else(RequestId::generate);
        req.extensions_mut().insert(id.clone());
        Ok(id)
    }
}

impl Request {
    /// The ID assigned by [`SetRequestId`], if the middleware ran.
    pub fn request_id(&self) -> Option<&RequestId> {
        self.extensions().get::<RequestId>()
    }
}

type Generator = Arc<dyn Fn() -> String + Send + Sync>;

/// Middleware that assigns every request a [`RequestId`].
///
/// - reuses a valid incoming ID (at most 128 chars of `[A-Za-z0-9._:-]`)
///   unless [`trust_incoming(false)`](Self::trust_incoming); invalid ones
///   are replaced,
/// - stores it in the request extensions and request header,
/// - sets the response header (unless the handler already set it).
///
/// Install it globally so 404s get an ID too:
///
/// ```
/// use zapline::prelude::*;
/// use zapline::middleware::SetRequestId;
///
/// let app = App::new().wrap(
///     SetRequestId::new()
///         .header("x-correlation-id")
///         .trust_incoming(false),
/// );
/// ```
#[derive(Clone)]
pub struct SetRequestId {
    header: HeaderName,
    generator: Option<Generator>,
    trust_incoming: bool,
}

impl Default for SetRequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl SetRequestId {
    /// Use `x-request-id`, trust valid incoming IDs, generate UUIDv7 IDs.
    pub fn new() -> Self {
        SetRequestId {
            header: X_REQUEST_ID,
            generator: None,
            trust_incoming: true,
        }
    }

    /// Read and write a different header.
    ///
    /// # Panics
    /// If `name` is not a valid header name.
    pub fn header(mut self, name: impl AsRef<str>) -> Self {
        self.header = HeaderName::from_bytes(name.as_ref().as_bytes())
            .unwrap_or_else(|_| panic!("invalid request ID header name {:?}", name.as_ref()));
        self
    }

    /// Generate IDs with `f` instead. Values that fail validation are
    /// replaced by a default ID.
    pub fn generator<F>(mut self, f: F) -> Self
    where
        F: Fn() -> String + Send + Sync + 'static,
    {
        self.generator = Some(Arc::new(f));
        self
    }

    /// Whether to reuse a valid incoming ID (default `true`). Turn it off
    /// when clients are untrusted and IDs must be server-generated.
    pub fn trust_incoming(mut self, trust: bool) -> Self {
        self.trust_incoming = trust;
        self
    }

    fn make_id(&self) -> RequestId {
        self.generator
            .as_ref()
            .and_then(|g| RequestId::parse(&g()))
            .unwrap_or_else(RequestId::generate)
    }
}

impl Middleware for SetRequestId {
    fn handle(&self, mut req: Request, next: Next) -> BoxFuture<Response> {
        let incoming = self
            .trust_incoming
            .then(|| {
                req.headers()
                    .get(&self.header)
                    .and_then(RequestId::from_header)
            })
            .flatten();
        let id = match incoming {
            Some(id) => id,
            None => {
                let id = self.make_id();
                req.headers_mut().insert(self.header.clone(), id.0.clone());
                id
            }
        };
        let value = id.0.clone();
        req.extensions_mut().insert(id);
        let header = self.header.clone();
        let fut = next.run(req);
        Box::pin(async move {
            let mut res = fut.await;
            res.headers_mut().entry(header).or_insert(value);
            res
        })
    }
}

#[inline]
fn is_valid(b: &[u8]) -> bool {
    !b.is_empty()
        && b.len() <= MAX_LEN
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

/// A UUIDv7 string: 48-bit Unix milliseconds, version, 74 random bits.
fn uuid_v7() -> HeaderValue {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let rand = fastrand::u128(..);
    let mut v: u128 = (u128::from(ms) & 0xFFFF_FFFF_FFFF) << 80;
    v |= 0x7 << 76; // version
    v |= ((rand >> 64) & 0xFFF) << 64; // rand_a
    v |= 0b10 << 62; // variant
    v |= rand & 0x3FFF_FFFF_FFFF_FFFF; // rand_b

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 36];
    let mut pos = 0;
    for i in 0..32 {
        if matches!(i, 8 | 12 | 16 | 20) {
            out[pos] = b'-';
            pos += 1;
        }
        out[pos] = HEX[((v >> (124 - i * 4)) & 0xF) as usize];
        pos += 1;
    }
    HeaderValue::from_bytes(&out).unwrap_or(HeaderValue::from_static("0"))
}
