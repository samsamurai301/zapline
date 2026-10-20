//! Server-Sent Events.
//!
//! Return [`Sse`] from a handler with a stream of [`Event`]s. Each event is
//! written to the client as soon as the stream yields it.
//!
//! ```
//! use std::time::Duration;
//! use futures_util::stream::{self, Stream, StreamExt};
//! use zapline::prelude::*;
//! use zapline::sse::{Event, KeepAlive, Sse};
//!
//! async fn ticks() -> Sse<impl Stream<Item = Event>> {
//!     let events = stream::iter(1..=3).map(|n| Event::default().event("tick").data(n.to_string()));
//!     Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
//! }
//!
//! let app = App::new().get("/ticks", ticks);
//! ```
//!
//! Streams may yield either plain [`Event`]s or `Result<Event, E>`; an `Err`
//! ends the response (the connection is closed mid-stream).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use futures_core::Stream;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::HeaderValue;
use http_body::Frame;
use tokio::time::{Instant, Sleep};

use crate::body::{Body, BoxError};
use crate::response::{IntoResponse, Response};

/// A Server-Sent Events response (`text/event-stream`).
///
/// Sets `Cache-Control: no-cache` and `X-Accel-Buffering: no` so proxies
/// such as nginx don't buffer the stream.
#[must_use]
pub struct Sse<S> {
    stream: S,
    keep_alive: Option<KeepAlive>,
}

impl<S> Sse<S>
where
    S: Stream + Send + 'static,
    S::Item: SseItem,
{
    /// An SSE response from a stream of `Event` or `Result<Event, E>`.
    pub fn new(stream: S) -> Self {
        Sse {
            stream,
            keep_alive: None,
        }
    }

    /// Send a comment frame whenever the stream has been idle for the
    /// keep-alive interval, so proxies and clients don't time out.
    pub fn keep_alive(mut self, keep_alive: KeepAlive) -> Self {
        self.keep_alive = Some(keep_alive);
        self
    }
}

impl<S> fmt::Debug for Sse<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sse")
            .field("keep_alive", &self.keep_alive)
            .finish_non_exhaustive()
    }
}

impl<S> IntoResponse for Sse<S>
where
    S: Stream + Send + 'static,
    S::Item: SseItem,
{
    fn into_response(self) -> Response {
        let timer = self.keep_alive.map(|config| KeepAliveTimer {
            sleep: Box::pin(tokio::time::sleep(config.interval)),
            config,
        });
        let body = SseBody {
            stream: self.stream,
            keep_alive: timer,
            done: false,
        };
        let mut res = Response::new(Body::Stream(Box::pin(body)));
        let h = res.headers_mut();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        h.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        h.insert("x-accel-buffering", HeaderValue::from_static("no"));
        res
    }
}

/// An item an [`Sse`] stream can yield: an [`Event`] or `Result<Event, E>`.
pub trait SseItem: private::Sealed {
    #[doc(hidden)]
    fn into_event(self) -> Result<Event, BoxError>;
}

impl SseItem for Event {
    #[inline]
    fn into_event(self) -> Result<Event, BoxError> {
        Ok(self)
    }
}

impl<E: Into<BoxError>> SseItem for Result<Event, E> {
    #[inline]
    fn into_event(self) -> Result<Event, BoxError> {
        self.map_err(Into::into)
    }
}

mod private {
    pub trait Sealed {}
    impl Sealed for super::Event {}
    impl<E> Sealed for Result<super::Event, E> {}
}

/// One Server-Sent Event.
///
/// Fields are written in the order the builder methods are called.
/// Multi-line `data` and `comment` values become one line per line of text
/// (`\n`, `\r\n` and `\r` all count as line breaks), so text can never
/// inject extra fields.
///
/// `event` and `id` must be single-line (and `id` must not contain NUL).
/// Invalid values don't panic: the event is marked invalid
/// ([`is_valid`](Self::is_valid)) and the stream ends with an error when it
/// reaches the client.
///
/// ```
/// use std::time::Duration;
/// use zapline::sse::Event;
///
/// let ev = Event::default()
///     .event("update")
///     .id("42")
///     .retry(Duration::from_secs(3))
///     .data("line one\nline two");
/// assert!(ev.is_valid());
/// ```
#[derive(Clone, Default)]
pub struct Event {
    buf: BytesMut,
    invalid: Option<&'static str>,
}

impl Event {
    /// Append `data` (one `data:` line per line of text).
    pub fn data(mut self, data: impl AsRef<str>) -> Self {
        for line in lines(data.as_ref()) {
            self.field("data", line);
        }
        self
    }

    /// Append `value` serialized as compact JSON on one `data:` line.
    #[cfg(feature = "json")]
    pub fn json_data(mut self, value: impl serde::Serialize) -> Result<Self, crate::Error> {
        let json = serde_json::to_string(&value)
            .map_err(|e| crate::Error::internal("failed to serialize SSE data").with_source(e))?;
        self.field("data", &json);
        Ok(self)
    }

    /// Set the event type (`event:`), dispatched to `addEventListener(name)`
    /// in the browser.
    pub fn event(mut self, name: impl AsRef<str>) -> Self {
        let name = name.as_ref();
        if name.contains(['\r', '\n']) {
            self.invalid = Some("SSE `event` must not contain line breaks");
        } else {
            self.field("event", name);
        }
        self
    }

    /// Set the event id (`id:`), echoed back by the browser in
    /// `Last-Event-ID` when it reconnects.
    pub fn id(mut self, id: impl AsRef<str>) -> Self {
        let id = id.as_ref();
        if id.contains(['\r', '\n', '\0']) {
            self.invalid = Some("SSE `id` must not contain line breaks or NUL");
        } else {
            self.field("id", id);
        }
        self
    }

    /// Set the client's reconnection delay (`retry:`, in milliseconds).
    pub fn retry(mut self, delay: Duration) -> Self {
        self.field("retry", &delay.as_millis().to_string());
        self
    }

    /// Append a comment (`:text`), ignored by clients.
    pub fn comment(mut self, text: impl AsRef<str>) -> Self {
        for line in lines(text.as_ref()) {
            self.buf.reserve(line.len() + 2);
            self.buf.put_u8(b':');
            self.buf.put_slice(line.as_bytes());
            self.buf.put_u8(b'\n');
        }
        self
    }

    /// `false` if `event` or `id` was given an invalid value.
    pub fn is_valid(&self) -> bool {
        self.invalid.is_none()
    }

    /// The event's wire format, terminated by a blank line.
    pub fn to_bytes(&self) -> Result<Bytes, crate::Error> {
        self.clone().finish().map_err(crate::Error::internal)
    }

    fn field(&mut self, name: &str, value: &str) {
        self.buf.reserve(name.len() + value.len() + 3);
        self.buf.put_slice(name.as_bytes());
        self.buf.put_slice(b": ");
        self.buf.put_slice(value.as_bytes());
        self.buf.put_u8(b'\n');
    }

    fn finish(mut self) -> Result<Bytes, &'static str> {
        if let Some(msg) = self.invalid {
            return Err(msg);
        }
        self.buf.put_u8(b'\n');
        Ok(self.buf.freeze())
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("wire", &String::from_utf8_lossy(&self.buf))
            .field("invalid", &self.invalid)
            .finish()
    }
}

/// Split on `\r\n`, `\n` and `\r` (the SSE line terminators). An empty string
/// is one empty line, and a trailing break yields a trailing empty line.
fn lines(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(s);
    std::iter::from_fn(move || {
        let r = rest?;
        match r.find(['\r', '\n']) {
            Some(i) => {
                let skip = if r[i..].starts_with("\r\n") { 2 } else { 1 };
                rest = Some(&r[i + skip..]);
                Some(&r[..i])
            }
            None => {
                rest = None;
                Some(r)
            }
        }
    })
}

/// Periodic comment frames sent while an [`Sse`] stream is idle.
///
/// ```
/// use std::time::Duration;
/// use zapline::sse::KeepAlive;
///
/// let ka = KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive");
/// ```
#[derive(Clone, Debug)]
pub struct KeepAlive {
    interval: Duration,
    frame: Bytes,
}

impl Default for KeepAlive {
    fn default() -> Self {
        Self::new()
    }
}

impl KeepAlive {
    /// Every 15 seconds, an empty comment (`:\n\n`).
    pub fn new() -> Self {
        KeepAlive {
            interval: Duration::from_secs(15),
            frame: Bytes::from_static(b":\n\n"),
        }
    }

    /// How long the stream may be idle before a keep-alive is sent
    /// (at least 1 ms).
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval.max(Duration::from_millis(1));
        self
    }

    /// The comment text (`:text\n\n`).
    pub fn text(mut self, text: impl AsRef<str>) -> Self {
        let ev = Event::default().comment(text);
        let mut buf = ev.buf;
        buf.put_u8(b'\n');
        self.frame = buf.freeze();
        self
    }
}

struct KeepAliveTimer {
    config: KeepAlive,
    sleep: Pin<Box<Sleep>>,
}

impl KeepAliveTimer {
    fn reset(&mut self) {
        let at = Instant::now() + self.config.interval;
        self.sleep.as_mut().reset(at);
    }
}

pin_project_lite::pin_project! {
    struct SseBody<S> {
        #[pin]
        stream: S,
        keep_alive: Option<KeepAliveTimer>,
        done: bool,
    }
}

impl<S> http_body::Body for SseBody<S>
where
    S: Stream,
    S::Item: SseItem,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.project();
        if *this.done {
            return Poll::Ready(None);
        }
        match this.stream.poll_next(cx) {
            Poll::Ready(Some(item)) => {
                let event = item
                    .into_event()
                    .and_then(|ev| ev.finish().map_err(invalid));
                match event {
                    Ok(bytes) => {
                        if let Some(ka) = this.keep_alive {
                            ka.reset();
                        }
                        Poll::Ready(Some(Ok(Frame::data(bytes))))
                    }
                    Err(e) => {
                        *this.done = true;
                        Poll::Ready(Some(Err(e)))
                    }
                }
            }
            Poll::Ready(None) => {
                *this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                let Some(ka) = this.keep_alive else {
                    return Poll::Pending;
                };
                ready!(ka.sleep.as_mut().poll(cx));
                ka.reset();
                Poll::Ready(Some(Ok(Frame::data(ka.config.frame.clone()))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }
}

fn invalid(msg: &'static str) -> BoxError {
    report_invalid(msg);
    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, msg))
}

#[cfg(feature = "tracing")]
fn report_invalid(msg: &str) {
    tracing::error!(error = msg, "invalid server-sent event; closing the stream");
}

#[cfg(not(feature = "tracing"))]
fn report_invalid(msg: &str) {
    eprintln!("[zapline] invalid server-sent event, closing the stream: {msg}");
}
