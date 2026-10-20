//! Multipart form data (`multipart/form-data`): HTML forms with file uploads.
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::multipart::Multipart;
//!
//! async fn upload(mut form: Multipart) -> Result<String> {
//!     let mut summary = Vec::new();
//!     while let Some(field) = form.next_field().await? {
//!         let name = field.name().unwrap_or("").to_owned();
//!         match field.file_name().map(str::to_owned) {
//!             Some(file) => {
//!                 let data = field.bytes().await?;
//!                 summary.push(format!("{name}: {file} ({} bytes)", data.len()));
//!             }
//!             None => summary.push(format!("{name} = {}", field.text().await?)),
//!         }
//!     }
//!     Ok(summary.join("\n"))
//! }
//!
//! let app = App::new().post("/upload", upload);
//! ```
//!
//! # Limits
//!
//! The whole body is bounded by the app's body limit
//! ([`App::body_limit`](crate::App::body_limit), 2 MiB by default) unless a
//! [`MultipartConfig`] says otherwise. The body is streamed through the
//! parser, never buffered as a whole. A [`MultipartConfig`] is looked up, in
//! order, from the matched route's metadata ([`Routes::meta`]) and from app
//! state ([`App::state`](crate::App::state)):
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::multipart::{Multipart, MultipartConfig};
//!
//! async fn upload(_form: Multipart) {}
//!
//! let app = App::new()
//!     // App-wide defaults...
//!     .state(MultipartConfig::new().max_field_size(256 * 1024))
//!     // ...and a per-route override for the big upload endpoint.
//!     .post("/videos", upload)
//!     .meta(MultipartConfig::new().body_limit(512 * 1024 * 1024).max_fields(4));
//! ```
//!
//! Errors map to `415` (not `multipart/form-data`), `400` (missing boundary,
//! malformed body, unexpected field) and `413` (body, field or field-count
//! limit exceeded), each with a stable `code`.
//!
//! [`Routes::meta`]: crate::Routes::meta

use std::fmt;
use std::marker::PhantomData;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{HeaderMap, StatusCode};
use tokio::io::AsyncWriteExt;

use crate::extract::FromRequest;
use crate::request::Request;
use crate::stream::{BodyStream, CompatError};
use crate::Error;

/// Limits for the [`Multipart`] extractor. Register it as app state or as
/// route metadata (see the [module docs](self)).
#[derive(Clone, Debug, Default)]
pub struct MultipartConfig {
    body_limit: Option<usize>,
    max_field_size: Option<u64>,
    field_limits: Vec<(String, u64)>,
    allowed_fields: Option<Vec<String>>,
    max_fields: Option<usize>,
}

impl MultipartConfig {
    /// No limits beyond the app's body limit.
    pub fn new() -> Self {
        Self::default()
    }

    /// Maximum size of the whole request body (default: the app's body
    /// limit).
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = Some(bytes);
        self
    }

    /// Maximum size of any single field.
    pub fn max_field_size(mut self, bytes: u64) -> Self {
        self.max_field_size = Some(bytes);
        self
    }

    /// Maximum size of the field called `name` (overrides
    /// [`max_field_size`](Self::max_field_size)).
    pub fn field_limit(mut self, name: impl Into<String>, bytes: u64) -> Self {
        self.field_limits.push((name.into(), bytes));
        self
    }

    /// Only accept these field names; others fail with `400`.
    pub fn allowed_fields<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_fields = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Maximum number of fields; more fail with `413`.
    pub fn max_fields(mut self, n: usize) -> Self {
        self.max_fields = Some(n);
        self
    }

    fn constraints(&self) -> multer::Constraints {
        let mut size = multer::SizeLimit::new();
        if let Some(n) = self.max_field_size {
            size = size.per_field(n);
        }
        for (name, n) in &self.field_limits {
            size = size.for_field(name.clone(), *n);
        }
        let c = multer::Constraints::new().size_limit(size);
        match &self.allowed_fields {
            Some(names) => c.allowed_fields(names.clone()),
            None => c,
        }
    }
}

/// Extractor for a `multipart/form-data` body, parsed as a stream of
/// [`Field`]s. Must be the last handler argument.
///
/// Only one field can be read at a time: a [`Field`] borrows the
/// `Multipart`, so drop (or consume) it before calling
/// [`next_field`](Self::next_field) again.
pub struct Multipart {
    inner: multer::Multipart<'static>,
    max_fields: Option<usize>,
    fields: usize,
}

impl fmt::Debug for Multipart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Multipart")
            .field("fields_read", &self.fields)
            .finish_non_exhaustive()
    }
}

impl Multipart {
    /// The next field, or `None` after the last one.
    pub async fn next_field(&mut self) -> Result<Option<Field<'_>>, Error> {
        let Some(inner) = self.inner.next_field().await.map_err(map_err)? else {
            return Ok(None);
        };
        self.fields += 1;
        if self.max_fields.is_some_and(|max| self.fields > max) {
            return Err(
                Error::new(StatusCode::PAYLOAD_TOO_LARGE, "too many multipart fields")
                    .with_code("multipart_too_many_fields"),
            );
        }
        Ok(Some(Field {
            inner,
            _multipart: PhantomData,
        }))
    }
}

impl FromRequest for Multipart {
    async fn from_request(mut req: Request) -> Result<Self, Error> {
        let content_type = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let boundary = multer::parse_boundary(content_type).map_err(|e| match e {
            multer::Error::NoBoundary => {
                Error::bad_request("missing boundary in `Content-Type: multipart/form-data`")
                    .with_code("multipart_missing_boundary")
            }
            _ => Error::unsupported_media_type("expected `Content-Type: multipart/form-data`")
                .with_code("multipart_unsupported_media_type"),
        })?;

        let config = req
            .route()
            .and_then(|r| r.meta::<MultipartConfig>())
            .or_else(|| req.state::<MultipartConfig>())
            .cloned()
            .unwrap_or_default();
        let limit = config.body_limit.unwrap_or_else(|| req.body_limit());
        let declared = req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|n| n > limit as u64) {
            return Err(Error::payload_too_large());
        }

        let body = BodyStream::new(req.take_body(), limit);
        Ok(Multipart {
            inner: multer::Multipart::with_constraints(body, boundary, config.constraints()),
            max_fields: config.max_fields,
            fields: 0,
        })
    }
}

/// One part of a multipart body: a form value or an uploaded file.
///
/// Read it whole ([`bytes`](Self::bytes), [`text`](Self::text)), chunk by
/// chunk ([`chunk`](Self::chunk), or as a `Stream`), or write it straight to
/// disk ([`save_to`](Self::save_to)).
pub struct Field<'a> {
    inner: multer::Field<'static>,
    _multipart: PhantomData<&'a mut Multipart>,
}

impl fmt::Debug for Field<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Field")
            .field("name", &self.name())
            .field("file_name", &self.file_name())
            .field("content_type", &self.content_type())
            .finish_non_exhaustive()
    }
}

impl Field<'_> {
    /// The form field name (`Content-Disposition: form-data; name="..."`).
    pub fn name(&self) -> Option<&str> {
        self.inner.name()
    }

    /// The client-supplied file name, for file uploads.
    ///
    /// This is untrusted input: never use it as a path directly. See
    /// [`safe_file_name`](Self::safe_file_name).
    pub fn file_name(&self) -> Option<&str> {
        self.inner.file_name()
    }

    /// The file name reduced to a safe single path component: directories,
    /// control characters and `\ / : * ? " < > |` are removed and the result
    /// is at most 255 bytes. `None` if nothing usable remains.
    pub fn safe_file_name(&self) -> Option<String> {
        sanitize_file_name(self.file_name()?)
    }

    /// The part's `Content-Type`, if given (e.g. `image/png`).
    pub fn content_type(&self) -> Option<&str> {
        self.inner.content_type().map(|m| m.as_ref())
    }

    /// All headers of this part.
    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    /// Zero-based position of this field in the body.
    pub fn index(&self) -> usize {
        self.inner.index()
    }

    /// The next chunk of data, or `None` at the end of the field.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        self.inner.chunk().await.map_err(map_err)
    }

    /// The whole field, buffered.
    pub async fn bytes(self) -> Result<Bytes, Error> {
        self.inner.bytes().await.map_err(map_err)
    }

    /// The whole field as text (decoded using the part's charset, UTF-8 by
    /// default).
    pub async fn text(self) -> Result<String, Error> {
        self.inner.text().await.map_err(map_err)
    }

    /// The whole field parsed as JSON (`422` if invalid).
    #[cfg(feature = "json")]
    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, Error> {
        let bytes = self.bytes().await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::unprocessable(format!("invalid JSON in multipart field: {e}")))
    }

    /// Stream the field into a new file at `path` (created or truncated),
    /// returning the number of bytes written.
    pub async fn save_to(mut self, path: impl AsRef<Path>) -> Result<u64, Error> {
        let mut file = tokio::fs::File::create(path).await?;
        let mut written = 0u64;
        while let Some(chunk) = self.chunk().await? {
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        Ok(written)
    }
}

impl Stream for Field<'_> {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner)
            .poll_next(cx)
            .map(|item| item.map(|r| r.map_err(map_err)))
    }
}

fn map_err(e: multer::Error) -> Error {
    use multer::Error as E;
    match e {
        E::StreamReadFailed(inner) => match inner.downcast::<CompatError>() {
            Ok(body) => body.0,
            Err(other) => Error::bad_request("failed to read multipart body").with_source(other),
        },
        E::FieldSizeExceeded { limit, field_name } => Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "multipart field `{}` exceeds the limit of {limit} bytes",
                field_name.as_deref().unwrap_or("")
            ),
        )
        .with_code("multipart_field_too_large"),
        E::StreamSizeExceeded { .. } => Error::payload_too_large(),
        E::UnknownField { field_name } => Error::bad_request(format!(
            "unexpected multipart field `{}`",
            field_name.as_deref().unwrap_or("")
        ))
        .with_code("multipart_unknown_field"),
        other => Error::bad_request(format!("invalid multipart body: {other}"))
            .with_code("multipart_invalid"),
    }
}

fn sanitize_file_name(name: &str) -> Option<String> {
    // Keep only the last path component, whichever separator the client used.
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let trimmed = out.trim_matches(|c: char| c == ' ' || c == '.');
    if trimmed.is_empty() {
        return None;
    }
    out = trimmed.to_owned();
    if out.len() > 255 {
        let mut end = 255;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::sanitize_file_name as s;

    #[test]
    fn file_names() {
        assert_eq!(s("photo.png").as_deref(), Some("photo.png"));
        assert_eq!(s("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(s("C:\\Users\\a\\b.txt").as_deref(), Some("b.txt"));
        assert_eq!(s("..").as_deref(), None);
        assert_eq!(s("  .hidden ").as_deref(), Some("hidden"));
        assert_eq!(s("a\0b\nc?.txt").as_deref(), Some("abc.txt"));
        assert_eq!(s("dir/").as_deref(), None);
        assert_eq!(s(&"é".repeat(200)).map(|n| n.len()), Some(254));
    }
}
