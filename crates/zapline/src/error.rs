//! A single error type that any `std::error::Error` converts into via `?`.

use std::borrow::Cow;
use std::fmt::{self, Write};

use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, StatusCode};

use crate::response::{IntoResponse, Response};

#[cfg(feature = "json")]
type Details = serde_json::Value;

/// HTTP-aware error. Any `E: std::error::Error` converts into a 500 via `?`;
/// use the constructors for client errors.
///
/// For `500 Internal Server Error` the client always receives the generic
/// message `"internal server error"` (plus `code`, if set); the specific
/// message and source are logged. Other statuses send their message as is.
///
/// Rendered as JSON: `{"error": "...", "code": "...", "details": ...}` where
/// `code` and `details` are omitted when unset. Customize globally with
/// [`App::error_handler`](crate::App::error_handler). Extractor rejections
/// use the same shape, with the codes listed in
/// [`extract`](crate::extract#rejections).
///
/// ```
/// use zapline::prelude::*;
///
/// async fn transfer(Path(amount): Path<u64>) -> Result<&'static str> {
///     if amount > 1_000 {
///         // 409 {"error":"limit exceeded","code":"limit_exceeded","details":{"max":1000}}
///         return Err(Error::conflict("limit exceeded")
///             .with_code("limit_exceeded")
///             .with_details(serde_json::json!({ "max": 1_000 })));
///     }
///     let _fee: u64 = "1".parse()?; // any std error: a logged 500
///     Ok("done")
/// }
/// # let _ = App::new().post("/transfer/{amount}", transfer);
/// ```
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
        // Database errors carry meaning (missing row, duplicate key) that
        // maps to 4xx statuses. A specialized impl would overlap with this
        // blanket one, so recognize them here; the check is a TypeId compare
        // that the compiler resolves per `E`.
        #[cfg(any(feature = "postgres", feature = "sqlite"))]
        {
            let mut slot = Some(e);
            if let Some(db) =
                (&mut slot as &mut dyn std::any::Any).downcast_mut::<Option<sqlx::Error>>()
            {
                if let Some(err) = db.take() {
                    return crate::db::from_sqlx(err);
                }
            }
            let e = slot.expect("error taken only for sqlx::Error");
            return Error::internal(GENERIC_500).with_source(e);
        }
        #[allow(unreachable_code)]
        Error::internal(GENERIC_500).with_source(e)
    }
}

/// A [`zapline::Error`](Error) viewed as a `std::error::Error`, so it can
/// travel through `BoxError`-based APIs (response streams, multer) and be
/// recovered intact with `downcast`.
pub(crate) struct CompatError(pub(crate) Error);

impl fmt::Debug for CompatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for CompatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.message())
    }
}

impl std::error::Error for CompatError {}

/// Lets streams that yield [`zapline::Error`](Error) (e.g. a
/// [`BodyStream`](crate::stream::BodyStream), or SSE events built with
/// `json_data`) be used wherever a [`BoxError`](crate::BoxError) is
/// expected, such as [`Body::from_stream`](crate::Body::from_stream) and
/// [`Sse`](crate::sse::Sse).
impl From<Error> for crate::body::BoxError {
    fn from(e: Error) -> Self {
        Box::new(CompatError(e))
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
        push_json_str(&mut body, &self.message);
        if let Some(code) = &self.code {
            body.push_str(r#","code":"#);
            push_json_str(&mut body, code);
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
        // A 500 means the server is at fault: its message and cause are
        // diagnostics for operators (type names, routes, driver errors) and
        // are logged, never sent. Clients get a generic message plus `code`.
        let message = if self.status == StatusCode::INTERNAL_SERVER_ERROR {
            if self.message != GENERIC_500 || self.source.is_some() {
                log_server_error(self.status, &self.message, self.source.as_deref());
            }
            if dev_mode() {
                Cow::Owned(dev_message(&self.message, self.source.as_deref()))
            } else {
                Cow::Borrowed(GENERIC_500)
            }
        } else {
            if self.status.is_server_error() {
                if let Some(src) = &self.source {
                    log_server_error(self.status, &self.message, Some(src.as_ref()));
                }
            }
            self.message
        };
        let report = ErrorReport {
            status: self.status,
            message: message.into_owned(),
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
            // `HeaderMap::into_iter` yields the name only for the first value
            // of each header; later values come with `None`.
            let mut name = None;
            for (k, v) in extra.into_iter() {
                if k.is_some() {
                    name = k;
                }
                if let Some(n) = &name {
                    h.append(n.clone(), v);
                }
            }
        }
        res.extensions_mut().insert(report);
        res
    }
}

const GENERIC_500: &str = "internal server error";

/// Set to `1` to send the real message and cause chain of 500 errors to
/// clients instead of a generic message. `zapline dev` sets it; never set it
/// in production, where those details are for the logs only.
pub const ENV_DEV: &str = "ZAPLINE_DEV";

fn dev_mode() -> bool {
    static DEV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEV.get_or_init(|| std::env::var_os(ENV_DEV).is_some_and(|v| v == "1"))
}

fn dev_message(
    message: &str,
    source: Option<&(dyn std::error::Error + Send + Sync + 'static)>,
) -> String {
    let mut out = String::from(message);
    let mut next: Option<&(dyn std::error::Error + 'static)> = match source {
        Some(e) => Some(e),
        None => None,
    };
    while let Some(err) = next {
        let text = err.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        next = err.source();
    }
    out
}

#[cfg(feature = "tracing")]
fn log_server_error(
    status: StatusCode,
    message: &str,
    source: Option<&(dyn std::error::Error + Send + Sync)>,
) {
    match source {
        Some(err) => tracing::error!(status = status.as_u16(), error = %err, "{message}"),
        None => tracing::error!(status = status.as_u16(), "{message}"),
    }
}

#[cfg(not(feature = "tracing"))]
fn log_server_error(
    status: StatusCode,
    message: &str,
    source: Option<&(dyn std::error::Error + Send + Sync)>,
) {
    match source {
        Some(err) => eprintln!("[zapline] {status}: {message}: {err}"),
        None => eprintln!("[zapline] {status}: {message}"),
    }
}

pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    push_json_str(&mut out, s);
    out
}

/// Append `s` to `out` as a quoted JSON string.
pub(crate) fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    // Everything escaped is ASCII, so slicing at those bytes keeps
    // multi-byte characters intact, and unescaped runs are copied whole.
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        if !matches!(b, b'"' | b'\\' | 0..=0x1f) {
            continue;
        }
        out.push_str(&s[start..i]);
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            _ => {
                let _ = write!(out, "\\u{b:04x}");
            }
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::json_escape;

    #[test]
    fn json_escape_round_trips() {
        for s in [
            "",
            "not found",
            "a\"b\\c",
            "x\ny\r\tz\u{1}\u{1f}",
            "é ✓ 𝄞 \"é\"",
        ] {
            let escaped = json_escape(s);
            assert_eq!(serde_json::from_str::<String>(&escaped).unwrap(), s);
        }
        assert_eq!(json_escape("a\"\u{1}"), r#""a\"\u0001""#);
    }
}
