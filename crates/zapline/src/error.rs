//! A single error type that any `std::error::Error` converts into via `?`.

use std::borrow::Cow;
use std::fmt;

use http::StatusCode;

use crate::response::{IntoResponse, Response};

/// HTTP-aware error. Any `E: std::error::Error` converts into a 500 via `?`;
/// use the constructors for client errors.
pub struct Error {
    status: StatusCode,
    message: Cow<'static, str>,
    source: Option<crate::body::BoxError>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(status: StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Error {
            status,
            message: message.into(),
            source: None,
        }
    }
    pub fn bad_request(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }
    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden")
    }
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not found")
    }
    pub fn conflict(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::CONFLICT, msg)
    }
    pub fn payload_too_large() -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "payload too large")
    }
    pub fn unprocessable(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, msg)
    }
    pub fn internal(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn source_error(&self) -> Option<&(dyn std::error::Error + Send + Sync)> {
        self.source.as_deref()
    }
}

// Deliberately not implementing `std::error::Error` for `Error` so this blanket
// impl does not overlap with `From<T> for T` (same trick as `anyhow`).
impl<E: std::error::Error + Send + Sync + 'static> From<E> for Error {
    fn from(e: E) -> Self {
        Error {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: Cow::Borrowed("internal server error"),
            source: Some(Box::new(e)),
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Error");
        d.field("status", &self.status)
            .field("message", &self.message);
        if let Some(s) = &self.source {
            d.field("source", s);
        }
        d.finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Internal details stay in `source`; the client only sees `message`.
        if self.status.is_server_error() {
            if let Some(src) = &self.source {
                eprintln!("[zapline] {}: {src}", self.status);
            }
        }
        let body = format!(r#"{{"error":{}}}"#, json_escape(&self.message));
        let mut res = Response::new(body.into());
        *res.status_mut() = self.status;
        res.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        res
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
