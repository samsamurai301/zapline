//! Security headers: sensible browser hardening defaults in one line.
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::middleware::SecurityHeaders;
//!
//! let app = App::new().wrap(SecurityHeaders::default());
//! // or, for an HTTPS-only app without third-party assets:
//! let app = App::new().wrap(SecurityHeaders::strict());
//! ```

use std::sync::Arc;
use std::time::Duration;

use http::header::{self, HeaderName, HeaderValue};

use super::{Middleware, Next};
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::Response;

const PERMISSIONS_POLICY: &str = "accelerometer=(), camera=(), geolocation=(), gyroscope=(), \
     magnetometer=(), microphone=(), payment=(), usb=()";

const STRICT_CSP: &str = "default-src 'self'; base-uri 'self'; object-src 'none'; \
     frame-ancestors 'none'; form-action 'self'; img-src 'self' data:; \
     upgrade-insecure-requests";

/// Adds browser security headers to every response.
///
/// [`SecurityHeaders::default()`] sets:
///
/// | header | value |
/// |---|---|
/// | `X-Content-Type-Options` | `nosniff` |
/// | `X-Frame-Options` | `DENY` |
/// | `Referrer-Policy` | `strict-origin-when-cross-origin` |
/// | `Cross-Origin-Opener-Policy` | `same-origin` |
/// | `X-DNS-Prefetch-Control` | `off` |
/// | `Permissions-Policy` | camera, microphone, geolocation, ... disabled |
///
/// It sets no `Content-Security-Policy`, so docs pages that load assets
/// from a CDN keep working. [`SecurityHeaders::strict()`] adds HSTS, a
/// strict CSP and `Cross-Origin-Resource-Policy: same-origin`.
///
/// Headers the handler already set are never overwritten. Values are built
/// once, at construction.
///
/// ```
/// use std::time::Duration;
/// use zapline::middleware::SecurityHeaders;
///
/// let headers = SecurityHeaders::default()
///     .hsts(Duration::from_secs(31_536_000), true, false)
///     .csp("default-src 'self'")
///     .frame_options("SAMEORIGIN")
///     .remove("x-dns-prefetch-control");
/// ```
#[derive(Clone)]
pub struct SecurityHeaders {
    headers: Arc<Vec<(HeaderName, HeaderValue)>>,
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        let s = |v: &'static str| HeaderValue::from_static(v);
        SecurityHeaders {
            headers: Arc::new(vec![
                (header::X_CONTENT_TYPE_OPTIONS, s("nosniff")),
                (header::X_FRAME_OPTIONS, s("DENY")),
                (
                    header::REFERRER_POLICY,
                    s("strict-origin-when-cross-origin"),
                ),
                (
                    HeaderName::from_static("cross-origin-opener-policy"),
                    s("same-origin"),
                ),
                (header::X_DNS_PREFETCH_CONTROL, s("off")),
                (
                    HeaderName::from_static("permissions-policy"),
                    s(PERMISSIONS_POLICY),
                ),
            ]),
        }
    }
}

impl SecurityHeaders {
    /// Same as [`SecurityHeaders::default()`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The defaults plus HSTS (`max-age=63072000; includeSubDomains; preload`),
    /// a strict `Content-Security-Policy` and
    /// `Cross-Origin-Resource-Policy: same-origin`.
    ///
    /// Only use it for apps served exclusively over HTTPS.
    pub fn strict() -> Self {
        Self::default()
            .hsts(Duration::from_secs(63_072_000), true, true)
            .csp(STRICT_CSP)
            .header("cross-origin-resource-policy", "same-origin")
    }

    /// `Strict-Transport-Security`. Browsers ignore it over plain HTTP.
    pub fn hsts(self, max_age: Duration, include_subdomains: bool, preload: bool) -> Self {
        let mut v = format!("max-age={}", max_age.as_secs());
        if include_subdomains {
            v.push_str("; includeSubDomains");
        }
        if preload {
            v.push_str("; preload");
        }
        self.header(header::STRICT_TRANSPORT_SECURITY.as_str(), &v)
    }

    /// `Content-Security-Policy`.
    pub fn csp(self, policy: &str) -> Self {
        self.header(header::CONTENT_SECURITY_POLICY.as_str(), policy)
    }

    /// `Content-Security-Policy-Report-Only`, to trial a policy.
    pub fn csp_report_only(self, policy: &str) -> Self {
        self.header(header::CONTENT_SECURITY_POLICY_REPORT_ONLY.as_str(), policy)
    }

    /// `X-Frame-Options` (`DENY` or `SAMEORIGIN`).
    pub fn frame_options(self, value: &str) -> Self {
        self.header(header::X_FRAME_OPTIONS.as_str(), value)
    }

    /// `Referrer-Policy`, e.g. `no-referrer`.
    pub fn referrer_policy(self, value: &str) -> Self {
        self.header(header::REFERRER_POLICY.as_str(), value)
    }

    /// `Permissions-Policy`, e.g. `camera=(), geolocation=(self)`.
    pub fn permissions_policy(self, value: &str) -> Self {
        self.header("permissions-policy", value)
    }

    /// Set (or replace) any header.
    ///
    /// # Panics
    /// If the name or value is invalid.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes())
            .unwrap_or_else(|_| panic!("invalid security header name {name:?}"));
        let value = HeaderValue::from_str(value)
            .unwrap_or_else(|_| panic!("invalid value for security header {name}: {value:?}"));
        let list = Arc::make_mut(&mut self.headers);
        match list.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => list.push((name, value)),
        }
        self
    }

    /// Stop sending a header (e.g. `"x-frame-options"`). Case-insensitive.
    pub fn remove(mut self, name: impl AsRef<str>) -> Self {
        let name = name.as_ref();
        Arc::make_mut(&mut self.headers).retain(|(n, _)| !n.as_str().eq_ignore_ascii_case(name));
        self
    }
}

impl Middleware for SecurityHeaders {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        let headers = self.headers.clone();
        let fut = next.run(req);
        Box::pin(async move {
            let mut res = fut.await;
            super::insert_missing(res.headers_mut(), &headers);
            res
        })
    }
}
