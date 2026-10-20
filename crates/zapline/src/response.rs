//! Turning handler return values into HTTP responses.

use bytes::Bytes;
use http::header::{HeaderName, CONTENT_TYPE, LOCATION};
use http::{HeaderValue, StatusCode};

use crate::body::Body;

pub type Response = http::Response<Body>;

/// Anything a handler can return.
pub trait IntoResponse {
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    #[inline]
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for () {
    #[inline]
    fn into_response(self) -> Response {
        Response::new(Body::Empty)
    }
}

impl IntoResponse for StatusCode {
    #[inline]
    fn into_response(self) -> Response {
        let mut r = Response::new(Body::Empty);
        *r.status_mut() = self;
        r
    }
}

impl IntoResponse for std::convert::Infallible {
    fn into_response(self) -> Response {
        match self {}
    }
}

const TEXT: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");
const OCTET: HeaderValue = HeaderValue::from_static("application/octet-stream");
const HTML: HeaderValue = HeaderValue::from_static("text/html; charset=utf-8");

#[inline]
fn with_type(body: Body, ct: HeaderValue) -> Response {
    let mut r = Response::new(body);
    r.headers_mut().insert(CONTENT_TYPE, ct);
    r
}

impl IntoResponse for &'static str {
    #[inline]
    fn into_response(self) -> Response {
        with_type(self.into(), TEXT)
    }
}
impl IntoResponse for String {
    #[inline]
    fn into_response(self) -> Response {
        with_type(self.into(), TEXT)
    }
}
impl IntoResponse for std::borrow::Cow<'static, str> {
    #[inline]
    fn into_response(self) -> Response {
        match self {
            std::borrow::Cow::Borrowed(s) => s.into_response(),
            std::borrow::Cow::Owned(s) => s.into_response(),
        }
    }
}
impl IntoResponse for Bytes {
    #[inline]
    fn into_response(self) -> Response {
        with_type(self.into(), OCTET)
    }
}
impl IntoResponse for Vec<u8> {
    #[inline]
    fn into_response(self) -> Response {
        with_type(self.into(), OCTET)
    }
}
impl IntoResponse for &'static [u8] {
    #[inline]
    fn into_response(self) -> Response {
        with_type(self.into(), OCTET)
    }
}

impl<T: IntoResponse, E: IntoResponse> IntoResponse for Result<T, E> {
    #[inline]
    fn into_response(self) -> Response {
        match self {
            Ok(v) => v.into_response(),
            Err(e) => e.into_response(),
        }
    }
}

impl<T: IntoResponse> IntoResponse for Option<T> {
    #[inline]
    fn into_response(self) -> Response {
        match self {
            Some(v) => v.into_response(),
            None => crate::Error::not_found().into_response(),
        }
    }
}

/// `(StatusCode, body)` sets the status.
impl<T: IntoResponse> IntoResponse for (StatusCode, T) {
    #[inline]
    fn into_response(self) -> Response {
        let mut r = self.1.into_response();
        *r.status_mut() = self.0;
        r
    }
}

/// `([(name, value); N], body)` adds headers.
impl<T: IntoResponse, const N: usize> IntoResponse for ([(HeaderName, HeaderValue); N], T) {
    #[inline]
    fn into_response(self) -> Response {
        let mut r = self.1.into_response();
        let h = r.headers_mut();
        for (k, v) in self.0 {
            h.insert(k, v);
        }
        r
    }
}

/// `(StatusCode, [(name, value); N], body)`.
impl<T: IntoResponse, const N: usize> IntoResponse
    for (StatusCode, [(HeaderName, HeaderValue); N], T)
{
    #[inline]
    fn into_response(self) -> Response {
        let mut r = (self.1, self.2).into_response();
        *r.status_mut() = self.0;
        r
    }
}

/// An HTML response.
pub struct Html<T>(pub T);

impl<T: Into<Body>> IntoResponse for Html<T> {
    #[inline]
    fn into_response(self) -> Response {
        with_type(self.0.into(), HTML)
    }
}

/// A redirect response (`303 See Other` by default).
///
/// A location that is not a valid header value (e.g. contains CR/LF, which
/// can happen with request-derived `?next=` URLs) never panics: the response
/// becomes a `500`. Validate user-supplied redirect targets (same-origin,
/// relative paths) before redirecting to them to avoid open redirects.
pub struct Redirect {
    status: StatusCode,
    location: Option<HeaderValue>,
}

impl Redirect {
    pub fn to(uri: &str) -> Self {
        Self::with_status(StatusCode::SEE_OTHER, uri)
    }
    pub fn temporary(uri: &str) -> Self {
        Self::with_status(StatusCode::TEMPORARY_REDIRECT, uri)
    }
    pub fn permanent(uri: &str) -> Self {
        Self::with_status(StatusCode::PERMANENT_REDIRECT, uri)
    }
    fn with_status(status: StatusCode, uri: &str) -> Self {
        Redirect {
            status,
            location: HeaderValue::try_from(uri).ok(),
        }
    }
}

impl IntoResponse for Redirect {
    fn into_response(self) -> Response {
        match self.location {
            Some(location) => {
                let mut r = self.status.into_response();
                r.headers_mut().insert(LOCATION, location);
                r
            }
            None => crate::Error::internal("invalid redirect location")
                .with_code("invalid_redirect")
                .into_response(),
        }
    }
}
