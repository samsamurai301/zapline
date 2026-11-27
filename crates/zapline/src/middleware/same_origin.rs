//! Cross-site request forgery protection from the browser's own headers:
//! no tokens in forms, nothing to configure for same-site apps.

use std::sync::Arc;

use http::header::{HOST, ORIGIN};
use http::{HeaderName, Method};

use super::{Middleware, Next};
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::Error;

const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

/// Rejects state-changing requests (`POST`, `PUT`, `PATCH`, `DELETE`, ...)
/// that a browser sent on behalf of another site, with
/// `403 cross_origin`. This stops cross-site request forgery for forms,
/// htmx and `fetch` alike, with nothing to add to templates.
///
/// Browsers say where a request comes from: `Sec-Fetch-Site` (every current
/// browser), otherwise `Origin`, which must match the request's host.
/// Requests with neither, such as `curl`, API clients and tests, are not
/// from a browser and pass. `GET`, `HEAD` and `OPTIONS` always pass, so keep
/// them free of side effects.
///
/// Unlike `SameSite` cookies alone, it also stops requests from sibling
/// subdomains (`same-site`), which `SameSite=Lax` lets through.
///
/// ```
/// use zapline::middleware::SameOrigin;
/// use zapline::prelude::*;
///
/// let app = App::new()
///     .wrap(SameOrigin::new())
///     .post("/todos", || async { "created" });
/// ```
///
/// A front end on another origin that calls this API with cookies needs
/// [`allow`](Self::allow); routes that take requests from other sites by
/// design (webhooks) need [`exempt`](Self::exempt).
#[derive(Clone, Debug, Default)]
pub struct SameOrigin {
    allowed: Arc<[String]>,
    exempt: Arc<[String]>,
}

impl SameOrigin {
    /// Only same-origin browser requests may change state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Also accept requests from these origins, e.g.
    /// `"https://app.example.com"` (scheme, host and port, no path).
    ///
    /// ```
    /// # use zapline::middleware::SameOrigin;
    /// let guard = SameOrigin::new().allow(["https://app.example.com"]);
    /// ```
    pub fn allow<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut all = self.allowed.to_vec();
        all.extend(
            origins
                .into_iter()
                .map(|o| o.into().trim_end_matches('/').to_ascii_lowercase()),
        );
        self.allowed = all.into();
        self
    }

    /// Skip the check for paths starting with these prefixes, e.g.
    /// `"/webhooks/"` for callbacks other services send.
    ///
    /// ```
    /// # use zapline::middleware::SameOrigin;
    /// let guard = SameOrigin::new().exempt(["/webhooks/"]);
    /// ```
    pub fn exempt<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut all = self.exempt.to_vec();
        all.extend(prefixes.into_iter().map(Into::into));
        self.exempt = all.into();
        self
    }

    /// Whether `req` may go through.
    pub fn allows(&self, req: &Request) -> bool {
        if matches!(
            *req.method(),
            Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
        ) {
            return true;
        }
        let path = req.uri().path();
        if self.exempt.iter().any(|p| path.starts_with(p.as_str())) {
            return true;
        }
        let header = |name: &HeaderName| req.headers().get(name).and_then(|v| v.to_str().ok());
        let origin = header(&ORIGIN);
        let listed = || {
            origin.is_some_and(|o| {
                self.allowed
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(o.trim_end_matches('/')))
            })
        };
        match header(&SEC_FETCH_SITE) {
            // `none`: typed into the address bar, a bookmark.
            Some("same-origin") | Some("none") => true,
            Some(_) => listed(),
            None => match origin {
                None => true,
                Some(o) => same_host(req, o) || listed(),
            },
        }
    }
}

/// Whether `origin` (`scheme://host[:port]`) names the host the request was
/// sent to (`Host`, or the HTTP/2 `:authority`).
fn same_host(req: &Request, origin: &str) -> bool {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));
    match (origin.split_once("://"), host) {
        (Some((_, origin_host)), Some(host)) => origin_host.eq_ignore_ascii_case(host),
        _ => false,
    }
}

impl Middleware for SameOrigin {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        if self.allows(&req) {
            next.run(req)
        } else {
            Box::pin(async {
                Error::new(
                    http::StatusCode::FORBIDDEN,
                    "this request came from another site",
                )
                .with_code("cross_origin")
                .into_response()
            })
        }
    }
}
