//! The request type handed to extractors, handlers and middleware.

use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Method, Uri};

use crate::body::ReqBody;

/// Application-wide data shared by every request.
pub(crate) struct Shared {
    pub(crate) states: Arc<[(TypeId, Box<dyn Any + Send + Sync>)]>,
    pub(crate) body_limit: usize,
}

impl Shared {
    #[inline]
    pub(crate) fn state<T: 'static>(&self) -> Option<&T> {
        // A linear scan beats hashing for the handful of state types apps register.
        let id = TypeId::of::<T>();
        self.states
            .iter()
            .find(|(t, _)| *t == id)
            .and_then(|(_, v)| v.downcast_ref::<T>())
    }
}

const INLINE_PARAMS: usize = 4;

/// Matched route parameters, stored as byte ranges into the request path so
/// routing never allocates for routes with up to four parameters.
#[derive(Clone, Default)]
pub(crate) struct Params {
    len: u8,
    inline: [(&'static str, u32, u32); INLINE_PARAMS],
    spill: Vec<(&'static str, u32, u32)>,
}

impl Params {
    #[inline]
    pub(crate) fn push(&mut self, name: &'static str, start: usize, end: usize) {
        let entry = (name, start as u32, end as u32);
        if (self.len as usize) < INLINE_PARAMS {
            self.inline[self.len as usize] = entry;
            self.len += 1;
        } else {
            self.spill.push(entry);
        }
    }

    #[inline]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &(&'static str, u32, u32)> {
        self.inline[..self.len as usize]
            .iter()
            .chain(self.spill.iter())
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len as usize + self.spill.len()
    }
}

/// An incoming HTTP request.
pub struct Request {
    pub(crate) head: http::request::Parts,
    pub(crate) body: Option<ReqBody>,
    pub(crate) params: Params,
    pub(crate) shared: Arc<Shared>,
}

impl Request {
    #[inline]
    pub fn method(&self) -> &Method {
        &self.head.method
    }
    #[inline]
    pub fn uri(&self) -> &Uri {
        &self.head.uri
    }
    #[inline]
    pub fn path(&self) -> &str {
        self.head.uri.path()
    }
    #[inline]
    pub fn query_string(&self) -> Option<&str> {
        self.head.uri.query()
    }
    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.head.headers
    }
    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head.headers
    }
    /// Typed per-request storage, e.g. for values set by middleware.
    #[inline]
    pub fn extensions(&self) -> &http::Extensions {
        &self.head.extensions
    }
    #[inline]
    pub fn extensions_mut(&mut self) -> &mut http::Extensions {
        &mut self.head.extensions
    }
    /// Replace the request URI. Only meaningful in global middleware, which
    /// runs before routing.
    pub fn set_uri(&mut self, uri: Uri) {
        self.head.uri = uri;
    }

    /// A route parameter by name, percent-decoded.
    pub fn param(&self, name: &str) -> Option<Cow<'_, str>> {
        let path = self.head.uri.path();
        self.params
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|&(_, s, e)| decode(&path[s as usize..e as usize]))
    }

    /// Application state registered with [`App::state`](crate::App::state).
    #[inline]
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.shared.state::<T>()
    }

    /// Take and buffer the body (respecting the app's body limit).
    /// Returns an empty body if it was already taken.
    pub async fn body_bytes(&mut self) -> crate::Result<Bytes> {
        match self.body.take() {
            Some(b) => b.collect_limited(self.shared.body_limit).await,
            None => Ok(Bytes::new()),
        }
    }

    pub(crate) fn param_values(&self) -> impl Iterator<Item = (&'static str, Cow<'_, str>)> {
        let path = self.head.uri.path();
        self.params
            .iter()
            .map(move |&(n, s, e)| (n, decode(&path[s as usize..e as usize])))
    }
}

#[inline]
pub(crate) fn decode(s: &str) -> Cow<'_, str> {
    if !s.as_bytes().contains(&b'%') {
        return Cow::Borrowed(s);
    }
    percent_encoding::percent_decode_str(s).decode_utf8_lossy()
}
