//! Streaming request and response bodies.
//!
//! - [`BodyStream`] reads the request body chunk by chunk without buffering
//!   it, while still enforcing the app's body limit.
//! - [`StreamBody`] and [`Body::from_stream`] turn any stream of byte chunks
//!   into a response.
//! - [`Ndjson`] (`json` feature) streams newline-delimited JSON.
//!
//! ```
//! use futures_util::stream;
//! use zapline::prelude::*;
//! use zapline::stream::{BodyStream, StreamBody};
//!
//! // Count uploaded bytes without holding the body in memory.
//! async fn upload(mut body: BodyStream) -> Result<String> {
//!     let mut total = 0;
//!     while let Some(chunk) = body.chunk().await? {
//!         total += chunk.len();
//!     }
//!     Ok(format!("{total} bytes"))
//! }
//!
//! // Stream a response chunk by chunk.
//! async fn download() -> StreamBody<impl futures_util::Stream<Item = Result<&'static str, std::io::Error>>> {
//!     StreamBody::new(stream::iter([Ok("hello "), Ok("world")])).content_type("text/plain")
//! }
//!
//! let app = App::new().post("/upload", upload).get("/download", download);
//! ```

use std::fmt;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use http::header::CONTENT_TYPE;
use http::HeaderValue;
use http_body::{Body as _, Frame};

use crate::body::{Body, BoxError, ReqBody};
use crate::extract::FromRequest;
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::Error;

// ---------------------------------------------------------------------------
// Request side

/// The request body as a stream of chunks, read without buffering.
///
/// The app's body limit ([`App::body_limit`](crate::App::body_limit)) is
/// enforced across chunks: once more bytes arrive than allowed, the stream
/// yields a `413 Payload Too Large` error and ends. A `Content-Length` that
/// already exceeds the limit fails on the first read. Use
/// [`with_limit`](Self::with_limit) to raise or lower the limit for one
/// handler (e.g. a large upload endpoint).
///
/// Must be the last handler argument. Implements [`Stream`], so
/// `futures_util::StreamExt` works too.
///
/// ```
/// use zapline::prelude::*;
/// use zapline::stream::BodyStream;
///
/// async fn upload(body: BodyStream) -> Result<String> {
///     let mut body = body.with_limit(100 * 1024 * 1024); // 100 MiB for this route
///     let mut n = 0;
///     while let Some(chunk) = body.chunk().await? {
///         n += chunk.len();
///     }
///     Ok(n.to_string())
/// }
/// ```
pub struct BodyStream {
    body: Option<ReqBody>,
    limit: usize,
    read: usize,
    started: bool,
}

impl BodyStream {
    pub(crate) fn new(body: Option<ReqBody>, limit: usize) -> Self {
        BodyStream {
            body,
            limit,
            read: 0,
            started: false,
        }
    }

    /// Override the maximum number of bytes this stream will accept.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// The maximum number of bytes this stream accepts.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes received so far.
    pub fn bytes_read(&self) -> usize {
        self.read
    }

    /// The next chunk, or `None` at the end of the body.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx))
            .await
            .transpose()
    }

    /// Fail (and end the stream) with `err`.
    fn fail(&mut self, err: Error) -> Poll<Option<Result<Bytes, Error>>> {
        self.body = None;
        Poll::Ready(Some(Err(err)))
    }

    fn too_large(&mut self) -> Poll<Option<Result<Bytes, Error>>> {
        let limit = self.limit;
        self.fail(
            Error::payload_too_large().with_source(std::io::Error::other(format!(
                "request body exceeds the limit of {limit} bytes"
            ))),
        )
    }
}

impl fmt::Debug for BodyStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyStream")
            .field("limit", &self.limit)
            .field("read", &self.read)
            .field("done", &self.body.is_none())
            .finish()
    }
}

impl Stream for BodyStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if !this.started {
            this.started = true;
            let declared = match &this.body {
                Some(ReqBody::Incoming(inc)) => inc.size_hint().lower(),
                Some(ReqBody::Full(b)) => b.len() as u64,
                None => 0,
            };
            if declared > this.limit as u64 {
                return this.too_large();
            }
        }
        loop {
            let chunk = match this.body.as_mut() {
                None => return Poll::Ready(None),
                Some(ReqBody::Full(b)) => {
                    let b = std::mem::take(b);
                    this.body = None;
                    if b.is_empty() {
                        return Poll::Ready(None);
                    }
                    b
                }
                Some(ReqBody::Incoming(inc)) => match ready!(Pin::new(inc).poll_frame(cx)) {
                    None => {
                        this.body = None;
                        return Poll::Ready(None);
                    }
                    Some(Err(e)) => {
                        return this
                            .fail(Error::bad_request("failed to read request body").with_source(e))
                    }
                    Some(Ok(frame)) => match frame.into_data() {
                        Ok(data) if !data.is_empty() => data,
                        // Trailers and empty frames carry no body bytes.
                        _ => continue,
                    },
                },
            };
            this.read = this.read.saturating_add(chunk.len());
            if this.read > this.limit {
                return this.too_large();
            }
            return Poll::Ready(Some(Ok(chunk)));
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.body {
            None => (0, Some(0)),
            Some(_) => (0, None),
        }
    }
}

impl FromRequest for BodyStream {
    async fn from_request(mut req: Request) -> Result<Self, Error> {
        let limit = req.body_limit();
        Ok(BodyStream::new(req.take_body(), limit))
    }
}

// `BodyStream` yields `zapline::Error`, which converts into `BoxError` (for
// `Body::from_stream`, `Sse`, multer) via the impl next to `Error`; multipart
// downcasts it back.
#[cfg(feature = "multipart")]
pub(crate) use crate::error::CompatError;

// ---------------------------------------------------------------------------
// Response side

pin_project_lite::pin_project! {
    /// Adapts a `Stream<Item = Result<impl Into<Bytes>, E>>` into an
    /// `http_body::Body`, skipping empty chunks.
    struct ChunkBody<S> {
        #[pin]
        stream: S,
    }
}

impl<S, B, E> http_body::Body for ChunkBody<S>
where
    S: Stream<Item = Result<B, E>>,
    B: Into<Bytes>,
    E: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let mut stream = self.project().stream;
        loop {
            return match ready!(stream.as_mut().poll_next(cx)) {
                Some(Ok(chunk)) => {
                    let chunk = chunk.into();
                    if chunk.is_empty() {
                        continue;
                    }
                    Poll::Ready(Some(Ok(Frame::data(chunk))))
                }
                Some(Err(e)) => Poll::Ready(Some(Err(e.into()))),
                None => Poll::Ready(None),
            };
        }
    }
}

impl Body {
    /// A streaming response body from any stream of byte chunks. Chunks can
    /// be anything convertible to [`Bytes`] (`String`, `Vec<u8>`,
    /// `&'static str`, ...). An `Err` item aborts the response mid-stream.
    ///
    /// ```
    /// use futures_util::stream;
    /// use zapline::{Body, Response};
    ///
    /// let chunks = stream::iter([Ok::<_, std::io::Error>("a"), Ok("b")]);
    /// let res = Response::new(Body::from_stream(chunks));
    /// ```
    pub fn from_stream<S, B, E>(stream: S) -> Body
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: Into<Bytes> + 'static,
        E: Into<BoxError> + 'static,
    {
        Body::Stream(Box::pin(ChunkBody { stream }))
    }
}

/// A streaming response built from a stream of byte chunks, with an optional
/// `Content-Type` (default: `application/octet-stream`).
///
/// The body is sent with chunked transfer encoding as items arrive.
///
/// ```
/// use futures_util::stream;
/// use zapline::stream::StreamBody;
///
/// let csv = stream::iter(["id,name\n", "1,ada\n"].map(Ok::<_, std::io::Error>));
/// let res = StreamBody::new(csv).content_type("text/csv");
/// ```
#[must_use]
pub struct StreamBody<S> {
    stream: S,
    content_type: Result<HeaderValue, ()>,
}

const OCTET_STREAM: HeaderValue = HeaderValue::from_static("application/octet-stream");

impl<S, B, E> StreamBody<S>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: Into<Bytes> + 'static,
    E: Into<BoxError> + 'static,
{
    /// A streaming response with `Content-Type: application/octet-stream`.
    pub fn new(stream: S) -> Self {
        StreamBody {
            stream,
            content_type: Ok(OCTET_STREAM),
        }
    }

    /// Set the `Content-Type`. An invalid header value turns the response
    /// into a `500` error instead of panicking.
    pub fn content_type(mut self, content_type: &str) -> Self {
        self.content_type = HeaderValue::from_str(content_type).map_err(|_| ());
        self
    }
}

impl<S, B, E> IntoResponse for StreamBody<S>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
    B: Into<Bytes> + 'static,
    E: Into<BoxError> + 'static,
{
    fn into_response(self) -> Response {
        let Ok(ct) = self.content_type else {
            return Error::internal("invalid Content-Type for StreamBody").into_response();
        };
        let mut res = Response::new(Body::from_stream(self.stream));
        res.headers_mut().insert(CONTENT_TYPE, ct);
        res
    }
}

#[cfg(feature = "json")]
pub use ndjson::Ndjson;

#[cfg(feature = "json")]
mod ndjson {
    use futures_core::Stream;
    use futures_util::StreamExt;
    use http::header::CONTENT_TYPE;
    use http::HeaderValue;
    use serde::Serialize;

    use crate::body::{Body, BoxError};
    use crate::response::{IntoResponse, Response};

    /// A streaming [newline-delimited JSON](https://github.com/ndjson/ndjson-spec)
    /// response (`application/x-ndjson`): each item is serialized as one
    /// compact JSON document followed by `\n`.
    ///
    /// ```
    /// use futures_util::stream;
    /// use serde::Serialize;
    /// use zapline::stream::Ndjson;
    ///
    /// #[derive(Serialize)]
    /// struct Row { id: u32 }
    ///
    /// async fn export() -> Ndjson {
    ///     Ndjson::new(stream::iter((1..=3).map(|id| Row { id })))
    /// }
    /// ```
    #[must_use]
    pub struct Ndjson {
        body: Body,
    }

    fn line<T: Serialize>(item: &T) -> Result<Vec<u8>, BoxError> {
        let mut buf = serde_json::to_vec(item)?;
        buf.push(b'\n');
        Ok(buf)
    }

    impl Ndjson {
        /// Stream infallible items. A serialization failure aborts the
        /// response mid-stream.
        pub fn new<S, T>(stream: S) -> Self
        where
            S: Stream<Item = T> + Send + 'static,
            T: Serialize,
        {
            Ndjson {
                body: Body::from_stream(stream.map(|item| line(&item))),
            }
        }

        /// Stream fallible items. An `Err` aborts the response mid-stream.
        pub fn try_new<S, T, E>(stream: S) -> Self
        where
            S: Stream<Item = Result<T, E>> + Send + 'static,
            T: Serialize,
            E: Into<BoxError>,
        {
            Ndjson {
                body: Body::from_stream(stream.map(|item| line(&item.map_err(Into::into)?))),
            }
        }
    }

    impl IntoResponse for Ndjson {
        fn into_response(self) -> Response {
            let mut res = Response::new(self.body);
            res.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson"),
            );
            res
        }
    }
}
