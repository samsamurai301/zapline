//! Path normalization: make `/users/` and `/users` (and `//users`) hit the
//! same route.
//!
//! Must be installed with [`App::wrap`](crate::App::wrap) (global
//! middleware), because it changes the path *before* routing.
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::middleware::NormalizePath;
//!
//! let app = App::new()
//!     .wrap(NormalizePath::trim_trailing_slash().merge_slashes(true))
//!     .get("/users", || async { "users" });
//! // GET /users/ and GET //users// are both served by the `/users` route.
//! ```

use http::header::{HeaderValue, LOCATION};
use http::uri::{PathAndQuery, Uri};
use http::StatusCode;

use super::{Middleware, Next};
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::{IntoResponse, Response};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slash {
    Trim,
    Append,
}

/// Normalizes request paths before routing, by rewriting the URI
/// (default) or by redirecting the client. The query string is preserved.
///
/// - [`trim_trailing_slash`](Self::trim_trailing_slash) (default):
///   `/users/` → `/users`. The root `/` is untouched. Directories served
///   by `ServeDir` (feature `fs`) then answer with their index file at
///   `/dir` instead of redirecting to `/dir/` (which would loop).
/// - [`append_trailing_slash`](Self::append_trailing_slash): `/users` →
///   `/users/`. Paths whose last segment contains a `.` (like
///   `/app.js`) are left alone. Route patterns never end in `/`, so this
///   is mainly useful with `redirect` (canonical URLs) or with catch-all
///   and fallback handlers such as static file serving.
/// - [`merge_slashes(true)`](Self::merge_slashes): `/a//b` → `/a/b`.
/// - [`redirect(status)`](Self::redirect): answer with a redirect to the
///   normalized URL instead of rewriting it.
#[derive(Clone, Copy, Debug)]
pub struct NormalizePath {
    slash: Slash,
    merge: bool,
    redirect: Option<StatusCode>,
}

impl Default for NormalizePath {
    fn default() -> Self {
        Self::trim_trailing_slash()
    }
}

impl NormalizePath {
    /// Remove trailing slashes (`/users/` → `/users`).
    pub fn trim_trailing_slash() -> Self {
        NormalizePath {
            slash: Slash::Trim,
            merge: false,
            redirect: None,
        }
    }

    /// Add a trailing slash (`/users` → `/users/`).
    pub fn append_trailing_slash() -> Self {
        NormalizePath {
            slash: Slash::Append,
            ..Self::trim_trailing_slash()
        }
    }

    /// Collapse repeated slashes (`/a//b` → `/a/b`). Default `false`.
    pub fn merge_slashes(mut self, merge: bool) -> Self {
        self.merge = merge;
        self
    }

    /// Redirect to the normalized URL with `status` (e.g.
    /// `StatusCode::PERMANENT_REDIRECT`) instead of rewriting it.
    ///
    /// # Panics
    /// If `status` is not a 3xx status.
    pub fn redirect(mut self, status: StatusCode) -> Self {
        assert!(
            status.is_redirection(),
            "NormalizePath::redirect needs a 3xx status, got {status}"
        );
        self.redirect = Some(status);
        self
    }

    /// The normalized path, or `None` when it is already normal.
    fn normalize(&self, path: &str) -> Option<String> {
        if !path.starts_with('/') {
            return None; // `*` (OPTIONS) or something unusual: leave it alone.
        }
        let merged = self.merge && path.contains("//");
        let needs_slash = match self.slash {
            Slash::Trim => path.len() > 1 && path.ends_with('/'),
            Slash::Append => !path.ends_with('/') && !last_segment(path).contains('.'),
        };
        if !merged && !needs_slash {
            return None;
        }
        let mut out = String::with_capacity(path.len() + 1);
        if merged {
            for c in path.chars() {
                if !(c == '/' && out.ends_with('/')) {
                    out.push(c);
                }
            }
        } else {
            out.push_str(path);
        }
        match self.slash {
            Slash::Trim => {
                let len = out.trim_end_matches('/').len().max(1);
                out.truncate(len);
            }
            Slash::Append => {
                if !out.ends_with('/') && !last_segment(&out).contains('.') {
                    out.push('/');
                }
            }
        }
        (out != path).then_some(out)
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

/// Request extension set by a [`NormalizePath`] that trims trailing
/// slashes: handlers must not redirect `/dir` to `/dir/` (it would be
/// trimmed again, looping forever). `ServeDir` then serves the index file
/// directly.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrailingSlashTrimmed;

impl Middleware for NormalizePath {
    fn handle(&self, mut req: Request, next: Next) -> BoxFuture<Response> {
        if let Some(path) = self.normalize(req.path()) {
            let target = match req.query_string() {
                Some(q) => format!("{path}?{q}"),
                None => path,
            };
            if let Some(status) = self.redirect {
                if let Some(res) = redirect(status, &target) {
                    return res;
                }
            } else if let Some(uri) = with_path(req.uri(), &target) {
                req.set_uri(uri);
            }
        }
        if self.slash == Slash::Trim {
            req.extensions_mut().insert(TrailingSlashTrimmed);
        }
        next.run(req)
    }
}

fn redirect(status: StatusCode, target: &str) -> Option<BoxFuture<Response>> {
    // Never emit `//host` or `/\host`: browsers treat those as another origin.
    let rest = target.trim_start_matches(['/', '\\']);
    let location = HeaderValue::try_from(format!("/{rest}")).ok()?;
    let mut res = status.into_response();
    res.headers_mut().insert(LOCATION, location);
    Some(Box::pin(std::future::ready(res)))
}

fn with_path(uri: &Uri, path_and_query: &str) -> Option<Uri> {
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(PathAndQuery::try_from(path_and_query).ok()?);
    Uri::from_parts(parts).ok()
}
