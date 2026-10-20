//! A single error type that any `std::error::Error` converts into via `?`.

use std::borrow::Cow;
use std::fmt;

use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};

use crate::response::{IntoResponse, Response};

#[cfg(feature = "json")]
type Details = serde_json::Value;

/// HTTP-aware error. Any `E: std::error::Error` converts into a 500 via `?`
/// (details are logged, never sent to the client); use the constructors for
/// client errors.
///
/// Rendered as JSON: `{"error": "...", "code": "...", "details": ...}` where
/// `code` and `details` are omitted when unset. Customize globally with
/// [`App::error_handler`](crate::App::error_handler).
pub struct Error {
    status: StatusCode,
    message: Cow<'static, str>,
    code: Option<Cow<'static, str>>,
    #[cfg(feature = "json")]
    details: Option<Details>,
    headers: Option<Box<HeaderMap>>,
    source: Option<crate::body::BoxError>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(status: StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Error {
            status,
            message: message.into(),
            code: None,
            #[cfg(feature = "json")]
            details: None,
            headers: None,
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
    pub fn method_not_allowed() -> Self {
        Self::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
    }
    pub fn conflict(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::CONFLICT, msg)
    }
    pub fn payload_too_large() -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "payload too large")
    }
    pub fn unsupported_media_type(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, msg)
    }
    pub fn unprocessable(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, msg)
    }
    pub fn too_many_requests() -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "too many requests")
    }
    pub fn internal(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
    pub fn service_unavailable(msg: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, msg)
    }

    /// A stable, machine-readable error code, e.g. `"user_not_found"`.
    pub fn with_code(mut self, code: impl Into<Cow<'static, str>>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Structured details (e.g. per-field validation errors).
    #[cfg(feature = "json")]
    pub fn with_details(mut self, details: impl serde::Serialize) -> Self {
        self.details = serde_json::to_value(details).ok();
        self
    }

    /// Add a response header (e.g. `WWW-Authenticate`, `Retry-After`).
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers
            .get_or_insert_with(Default::default)
            .append(name, value);
        self
    }

    /// Keep an underlying cause for logging (never sent to clients).
    pub fn with_source(mut self, source: impl Into<crate::body::BoxError>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }
    #[cfg(feature = "json")]
    pub fn details(&self) -> Option<&serde_json::Value> {
        self.details.as_ref()
    }
    pub fn source_error(&self) -> Option<&(dyn std::error::Error + Send + Sync)> {
        self.source.as_deref()
    }
}

// Deliberately not implementing `std::error::Error` for `Error` so this blanket
// impl does not overlap with `From<T> for T` (same trick as `anyhow`).
impl<E: std::error::Error + Send + Sync + 'static> From<E> for Error {
    fn from(e: E) -> Self {
        Error::internal("internal server error").with_source(e)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Error");
        d.field("status", &self.status)
            .field("message", &self.message);
        if let Some(c) = &self.code {
            d.field("code", c);
        }
        if let Some(s) = &self.source {
            d.field("source", s);
        }
        d.finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.status, self.message)?;
        if let Some(src) = &self.source {
            write!(f, " ({src})")?;
        }
        Ok(())
    }
}

/// What an error response was made from. Attached to every error response's
/// extensions so middleware and [`App::error_handler`](crate::App::error_handler)
/// can re-render it.
#[derive(Clone, Debug)]
pub struct ErrorReport {
    pub status: StatusCode,
    pub message: String,
    pub code: Option<String>,
    #[cfg(feature = "json")]
    pub details: Option<serde_json::Value>,
}

impl ErrorReport {
    /// The default JSON body for this report.
    pub fn to_json(&self) -> String {
        let mut body = String::with_capacity(32 + self.message.len());
        body.push_str(r#"{"error":"#);
        body.push_str(&json_escape(&self.message));
        if let Some(code) = &self.code {
            body.push_str(r#","code":"#);
            body.push_str(&json_escape(code));
        }
        #[cfg(feature = "json")]
        if let Some(d) = &self.details {
            body.push_str(r#","details":"#);
            body.push_str(&d.to_string());
        }
        body.push('}');
        body
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            if let Some(src) = &self.source {
                log_server_error(self.status, src.as_ref());
            }
        }
        let report = ErrorReport {
            status: self.status,
            message: self.message.into_owned(),
            code: self.code.map(Cow::into_owned),
            #[cfg(feature = "json")]
            details: self.details,
        };
        let mut res = Response::new(report.to_json().into());
        *res.status_mut() = self.status;
        let h = res.headers_mut();
        h.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(extra) = self.headers {
            for (k, v) in extra.into_iter() {
                if let Some(k) = k {
                    h.append(k, v);
                }
            }
        }
        res.extensions_mut().insert(report);
        res
    }
}

#[cfg(feature = "tracing")]
fn log_server_error(status: StatusCode, err: &(dyn std::error::Error + Send + Sync)) {
    tracing::error!(status = status.as_u16(), error = %err, "request failed");
}

#[cfg(not(feature = "tracing"))]
fn log_server_error(status: StatusCode, err: &(dyn std::error::Error + Send + Sync)) {
    eprintln!("[zapline] {status}: {err}");
}

pub(crate) fn json_escape(s: &str) -> String {
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
