//! WebSockets.
//!
//! Take a [`WebSocketUpgrade`] in a `GET` handler and call
//! [`on_upgrade`](WebSocketUpgrade::on_upgrade) with an async callback. The
//! handler returns `101 Switching Protocols`; the callback then runs in its
//! own task with the connected [`WebSocket`].
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::ws::{Message, WebSocket, WebSocketUpgrade};
//!
//! async fn echo(ws: WebSocketUpgrade) -> Response {
//!     ws.on_upgrade(|mut socket: WebSocket| async move {
//!         while let Some(Ok(msg)) = socket.recv().await {
//!             if msg.is_text() || msg.is_binary() {
//!                 if socket.send(msg).await.is_err() {
//!                     break;
//!                 }
//!             }
//!         }
//!     })
//! }
//!
//! let app = App::new().get("/ws", echo);
//! ```
//!
//! [`WebSocket`] implements `Stream` and `Sink`, so
//! `futures_util::StreamExt::split` gives independent reader and writer
//! halves (e.g. one task forwarding a broadcast channel while another reads).
//!
//! # Messages
//!
//! zapline has its own [`Message`] type (rather than re-exporting
//! tungstenite's) so the public API stays stable across tungstenite
//! releases. Text is a plain `String`; conversion is zero-copy apart from a
//! UTF-8 re-check.
//!
//! # Testing
//!
//! The in-process [`TestClient`](crate::test::TestClient) has no real
//! connection to upgrade. There, `on_upgrade` still validates the handshake
//! and returns the `101` response (so handshake behavior is testable), but
//! the callback is never run. Test the conversation itself over a real
//! socket with [`App::serve`](crate::App::serve) and a WebSocket client.
//!
//! WebSockets over HTTP/2 (RFC 8441 extended `CONNECT`) are not supported.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use base64::Engine as _;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::{Sink, SinkExt, StreamExt};
use http::header::{
    CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL,
    SEC_WEBSOCKET_VERSION, UPGRADE,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite as ts;
use tokio_tungstenite::WebSocketStream;

use crate::extract::FromRequestParts;
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::Error;

/// Default maximum incoming message size: 16 MiB.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 << 20;
/// Default maximum incoming frame size: 16 MiB.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 << 20;

// ---------------------------------------------------------------------------
// Upgrade extractor

/// Extractor for a WebSocket handshake request.
///
/// Validates the request (`GET`, `Connection: upgrade`, `Upgrade: websocket`,
/// `Sec-WebSocket-Version: 13`, a well-formed `Sec-WebSocket-Key`) and
/// rejects it otherwise:
///
/// | Problem | Status |
/// |---|---|
/// | Not a `GET` request | `400` (`ws_invalid_method`) |
/// | Missing `Connection: upgrade` / `Upgrade: websocket` | `426` with `Upgrade: websocket` (`ws_upgrade_required`) |
/// | `Sec-WebSocket-Version` is not `13` | `426` with `Sec-WebSocket-Version: 13` (`ws_unsupported_version`) |
/// | Missing or malformed `Sec-WebSocket-Key` | `400` (`ws_invalid_key`) |
///
/// ```
/// use zapline::prelude::*;
/// use zapline::ws::WebSocketUpgrade;
///
/// async fn chat(ws: WebSocketUpgrade) -> Response {
///     ws.protocols(["chat.v2", "chat.v1"])
///         .max_message_size(64 * 1024)
///         .on_upgrade(|socket| async move {
///             let _proto = socket.protocol(); // the negotiated subprotocol, if any
///         })
/// }
/// ```
pub struct WebSocketUpgrade {
    key: HeaderValue,
    offered: Vec<String>,
    selected: Option<HeaderValue>,
    on_upgrade: Option<OnUpgrade>,
    config: Config,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    max_message_size: Option<usize>,
    max_frame_size: Option<usize>,
    read_buffer_size: Option<usize>,
    write_buffer_size: Option<usize>,
    max_write_buffer_size: Option<usize>,
}

impl Config {
    fn tungstenite(self) -> ts::protocol::WebSocketConfig {
        let mut c = ts::protocol::WebSocketConfig::default()
            .max_message_size(self.max_message_size)
            .max_frame_size(self.max_frame_size);
        if let Some(n) = self.read_buffer_size {
            c = c.read_buffer_size(n);
        }
        if let Some(n) = self.write_buffer_size {
            c = c.write_buffer_size(n.min(usize::MAX - 1));
        }
        if let Some(n) = self.max_write_buffer_size {
            c = c.max_write_buffer_size(n);
        }
        // tungstenite panics unless max_write_buffer_size > write_buffer_size.
        if c.max_write_buffer_size <= c.write_buffer_size {
            c.max_write_buffer_size = c.write_buffer_size + 1;
        }
        c
    }
}

impl fmt::Debug for WebSocketUpgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocketUpgrade")
            .field("offered_protocols", &self.offered)
            .field("selected_protocol", &self.selected_protocol())
            .field("upgradable", &self.on_upgrade.is_some())
            .field("config", &self.config)
            .finish()
    }
}

impl WebSocketUpgrade {
    /// The subprotocols this server supports, in order of preference. The
    /// first one the client also offered (`Sec-WebSocket-Protocol`) is
    /// selected and echoed back; if none match, no protocol is selected and
    /// the client decides whether to proceed.
    pub fn protocols<I, P>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        self.selected = protocols
            .into_iter()
            .find(|p| self.offered.iter().any(|o| o == p.as_ref()))
            .and_then(|p| HeaderValue::from_str(p.as_ref()).ok());
        self
    }

    /// The subprotocols the client offered, in its order.
    pub fn requested_protocols(&self) -> impl Iterator<Item = &str> {
        self.offered.iter().map(String::as_str)
    }

    /// The negotiated subprotocol (after [`protocols`](Self::protocols)).
    pub fn selected_protocol(&self) -> Option<&str> {
        self.selected.as_ref().and_then(|v| v.to_str().ok())
    }

    /// Maximum size of an incoming message, after reassembling fragments
    /// (default [`DEFAULT_MAX_MESSAGE_SIZE`]). Larger messages close the
    /// connection with an error.
    pub fn max_message_size(mut self, bytes: usize) -> Self {
        self.config.max_message_size = Some(bytes);
        self
    }

    /// Maximum size of a single incoming frame (default
    /// [`DEFAULT_MAX_FRAME_SIZE`]).
    pub fn max_frame_size(mut self, bytes: usize) -> Self {
        self.config.max_frame_size = Some(bytes);
        self
    }

    /// Size of the per-connection read buffer (tungstenite default: 128 KiB).
    /// Lower it for many mostly-idle connections.
    pub fn read_buffer_size(mut self, bytes: usize) -> Self {
        self.config.read_buffer_size = Some(bytes);
        self
    }

    /// Buffer outgoing data until this many bytes are queued before writing
    /// (tungstenite default: 128 KiB; `0` writes every message eagerly).
    /// Every `send` flushes regardless.
    pub fn write_buffer_size(mut self, bytes: usize) -> Self {
        self.config.write_buffer_size = Some(bytes);
        self
    }

    /// Cap on the outgoing buffer when writes are failing (default:
    /// unlimited). Always kept above the write buffer size.
    pub fn max_write_buffer_size(mut self, bytes: usize) -> Self {
        self.config.max_write_buffer_size = Some(bytes);
        self
    }

    /// Finish the handshake: returns the `101 Switching Protocols` response
    /// and, once the connection is upgraded, runs `callback` in a new task.
    ///
    /// The callback may return `()` or `Result<(), E>`; an `Err` is logged.
    /// Without a real connection to upgrade (the in-process test client) the
    /// `101` response is still returned but the callback never runs.
    pub fn on_upgrade<F, Fut>(self, callback: F) -> Response
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: CallbackResult,
    {
        let accept = ts::handshake::derive_accept_key(self.key.as_bytes());
        let mut res = StatusCode::SWITCHING_PROTOCOLS.into_response();
        let h = res.headers_mut();
        h.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(UPGRADE, HeaderValue::from_static("websocket"));
        if let Ok(v) = HeaderValue::from_str(&accept) {
            h.insert(SEC_WEBSOCKET_ACCEPT, v);
        }
        if let Some(p) = &self.selected {
            h.insert(SEC_WEBSOCKET_PROTOCOL, p.clone());
        }

        if let Some(on_upgrade) = self.on_upgrade {
            let config = self.config.tungstenite();
            let protocol = self.selected;
            tokio::spawn(async move {
                let upgraded = match on_upgrade.await {
                    Ok(u) => u,
                    Err(e) => return log_upgrade_failed(&e),
                };
                let inner = WebSocketStream::from_raw_socket(
                    TokioIo::new(upgraded),
                    ts::protocol::Role::Server,
                    Some(config),
                )
                .await;
                if let Err(e) = callback(WebSocket { inner, protocol }).await.into_result() {
                    log_callback_error(&e);
                }
            });
        }
        res
    }
}

impl FromRequestParts for WebSocketUpgrade {
    async fn from_request_parts(req: &mut Request) -> Result<Self, Error> {
        if req.method() != Method::GET {
            return Err(
                Error::bad_request("WebSocket upgrade requires a GET request")
                    .with_code("ws_invalid_method"),
            );
        }
        let headers = req.headers();
        if !has_token(headers, &CONNECTION, "upgrade") || !has_token(headers, &UPGRADE, "websocket")
        {
            return Err(Error::new(
                StatusCode::UPGRADE_REQUIRED,
                "expected a WebSocket upgrade request (`Connection: upgrade`, `Upgrade: websocket`)",
            )
            .with_code("ws_upgrade_required")
            .with_header(UPGRADE, HeaderValue::from_static("websocket"))
            .with_header(CONNECTION, HeaderValue::from_static("upgrade")));
        }
        if headers.get(SEC_WEBSOCKET_VERSION).map(|v| v.as_bytes()) != Some(b"13") {
            return Err(Error::new(
                StatusCode::UPGRADE_REQUIRED,
                "unsupported WebSocket version (expected `Sec-WebSocket-Version: 13`)",
            )
            .with_code("ws_unsupported_version")
            .with_header(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13")));
        }
        let key = match headers.get(SEC_WEBSOCKET_KEY) {
            Some(k) if valid_key(k.as_bytes()) => k.clone(),
            _ => {
                return Err(Error::bad_request(
                    "missing or invalid `Sec-WebSocket-Key` (must be 16 base64-encoded bytes)",
                )
                .with_code("ws_invalid_key"))
            }
        };
        let offered = headers
            .get_all(SEC_WEBSOCKET_PROTOCOL)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(WebSocketUpgrade {
            key,
            offered,
            selected: None,
            on_upgrade: req.extensions_mut().remove::<OnUpgrade>(),
            config: Config {
                max_message_size: Some(DEFAULT_MAX_MESSAGE_SIZE),
                max_frame_size: Some(DEFAULT_MAX_FRAME_SIZE),
                read_buffer_size: None,
                write_buffer_size: None,
                max_write_buffer_size: None,
            },
        })
    }
}

/// Whether a comma-separated header (possibly repeated) contains `token`,
/// compared case-insensitively.
fn has_token(headers: &HeaderMap, name: &HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// RFC 6455: the key is a base64-encoded 16-byte nonce.
fn valid_key(key: &[u8]) -> bool {
    let mut out = [0u8; 18];
    key.len() == 24
        && matches!(
            base64::engine::general_purpose::STANDARD.decode_slice(key, &mut out),
            Ok(16)
        )
}

/// What an [`on_upgrade`](WebSocketUpgrade::on_upgrade) callback may
/// return: `()` or `Result<(), E: Display>` (errors are logged).
pub trait CallbackResult: private::Sealed {
    #[doc(hidden)]
    fn into_result(self) -> Result<(), String>;
}

impl CallbackResult for () {
    fn into_result(self) -> Result<(), String> {
        Ok(())
    }
}

impl<E: fmt::Display> CallbackResult for Result<(), E> {
    fn into_result(self) -> Result<(), String> {
        self.map_err(|e| e.to_string())
    }
}

mod private {
    pub trait Sealed {}
    impl Sealed for () {}
    impl<E> Sealed for Result<(), E> {}
}

#[cfg(feature = "tracing")]
fn log_upgrade_failed(e: &hyper::Error) {
    tracing::debug!(error = %e, "websocket upgrade failed");
}

#[cfg(not(feature = "tracing"))]
fn log_upgrade_failed(_: &hyper::Error) {}

#[cfg(feature = "tracing")]
fn log_callback_error(e: &str) {
    tracing::warn!(error = e, "websocket handler failed");
}

#[cfg(not(feature = "tracing"))]
fn log_callback_error(e: &str) {
    eprintln!("[zapline] websocket handler failed: {e}");
}

// ---------------------------------------------------------------------------
// Connected socket

/// A connected WebSocket (server side).
///
/// Read with [`recv`](Self::recv) (or as a `Stream`), write with
/// [`send`](Self::send) (or as a `Sink`). Pings are answered with pongs
/// automatically while you read; they are still delivered to you as
/// [`Message::Ping`].
pub struct WebSocket {
    inner: WebSocketStream<TokioIo<Upgraded>>,
    protocol: Option<HeaderValue>,
}

impl fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocket")
            .field("protocol", &self.protocol())
            .finish_non_exhaustive()
    }
}

impl WebSocket {
    /// The next message. `None` once the connection is closed (after the
    /// close handshake, or if the peer went away).
    pub async fn recv(&mut self) -> Option<Result<Message, Error>> {
        self.next().await
    }

    /// Send a message (anything convertible: `String`, `&str`, `Bytes`,
    /// `Vec<u8>`, [`Message`]) and flush it.
    pub async fn send(&mut self, msg: impl Into<Message>) -> Result<(), Error> {
        SinkExt::send(self, msg.into()).await
    }

    /// Start the close handshake with code `1000` (normal closure). Keep
    /// calling [`recv`](Self::recv) until it returns `None` to let the peer
    /// acknowledge, or just drop the socket.
    pub async fn close(&mut self) -> Result<(), Error> {
        self.close_with(close_code::NORMAL, "").await
    }

    /// Start the close handshake with a specific code and reason (the
    /// reason is truncated to the protocol limit of 123 bytes).
    pub async fn close_with(&mut self, code: u16, reason: &str) -> Result<(), Error> {
        let frame = CloseFrame {
            code,
            reason: reason.to_owned(),
        };
        self.send(Message::Close(Some(frame))).await
    }

    /// The subprotocol negotiated during the handshake, if any.
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_ref().and_then(|v| v.to_str().ok())
    }

    /// Serialize `value` as JSON and send it as a text message.
    #[cfg(feature = "json")]
    pub async fn send_json<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), Error> {
        let msg = Message::json(value)?;
        self.send(msg).await
    }

    /// Receive the next text or binary message and parse it as JSON,
    /// skipping pings and pongs. `None` once the connection is closed.
    #[cfg(feature = "json")]
    pub async fn recv_json<T: serde::de::DeserializeOwned>(&mut self) -> Option<Result<T, Error>> {
        loop {
            return match self.recv().await? {
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => None,
                Ok(msg) => Some(msg.parse_json()),
                Err(e) => Some(Err(e)),
            };
        }
    }
}

impl Stream for WebSocket {
    type Item = Result<Message, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match ready!(Pin::new(&mut self.inner).poll_next(cx)) {
                Some(Ok(msg)) => match Message::from_tungstenite(msg) {
                    Some(msg) => Poll::Ready(Some(Ok(msg))),
                    None => continue,
                },
                Some(Err(e)) => Poll::Ready(Some(Err(ws_error(e)))),
                None => Poll::Ready(None),
            };
        }
    }
}

impl Sink<Message> for WebSocket {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.inner).poll_ready(cx).map_err(ws_error)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Error> {
        Pin::new(&mut self.inner)
            .start_send(item.into_tungstenite())
            .map_err(ws_error)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(ws_error)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.inner).poll_close(cx).map_err(ws_error)
    }
}

/// Map a tungstenite error to a [`zapline::Error`](crate::Error) with a
/// stable code (`ws_closed`, `ws_message_too_large`, `ws_protocol_error`,
/// `ws_io_error`, `ws_error`).
fn ws_error(e: ts::Error) -> Error {
    use ts::Error as E;
    let (status, code) = match &e {
        E::ConnectionClosed | E::AlreadyClosed => (StatusCode::GONE, "ws_closed"),
        E::Capacity(_) | E::WriteBufferFull(_) => {
            (StatusCode::PAYLOAD_TOO_LARGE, "ws_message_too_large")
        }
        E::Protocol(_) | E::Utf8(_) | E::AttackAttempt => {
            (StatusCode::BAD_REQUEST, "ws_protocol_error")
        }
        E::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "ws_io_error"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "ws_error"),
    };
    Error::new(status, e.to_string())
        .with_code(code)
        .with_source(e)
}

// ---------------------------------------------------------------------------
// Messages

/// A WebSocket message.
///
/// ```
/// use zapline::ws::Message;
///
/// let msg = Message::text("hello");
/// assert_eq!(msg.as_text(), Some("hello"));
/// assert!(Message::from(vec![1u8, 2, 3]).is_binary());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A UTF-8 text message.
    Text(String),
    /// A binary message.
    Binary(Bytes),
    /// A ping (payload at most 125 bytes). Answered automatically.
    Ping(Bytes),
    /// A pong (payload at most 125 bytes).
    Pong(Bytes),
    /// A close frame, with an optional code and reason.
    Close(Option<CloseFrame>),
}

/// The payload of a close frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    /// Status code; see [`close_code`].
    pub code: u16,
    /// Human-readable reason (at most 123 bytes are sent).
    pub reason: String,
}

/// Common close codes (RFC 6455 §7.4.1).
pub mod close_code {
    /// Normal closure.
    pub const NORMAL: u16 = 1000;
    /// The endpoint is going away (server shutdown, page navigation).
    pub const AWAY: u16 = 1001;
    /// Protocol error.
    pub const PROTOCOL: u16 = 1002;
    /// Received a data type it cannot accept.
    pub const UNSUPPORTED: u16 = 1003;
    /// Received data inconsistent with the message type (e.g. bad UTF-8).
    pub const INVALID: u16 = 1007;
    /// Policy violation.
    pub const POLICY: u16 = 1008;
    /// Message too big.
    pub const SIZE: u16 = 1009;
    /// The client expected an extension the server didn't negotiate.
    pub const EXTENSION: u16 = 1010;
    /// Unexpected server error.
    pub const ERROR: u16 = 1011;
    /// The server is restarting.
    pub const RESTART: u16 = 1012;
    /// Try again later.
    pub const AGAIN: u16 = 1013;
}

impl Message {
    /// A text message.
    pub fn text(text: impl Into<String>) -> Self {
        Message::Text(text.into())
    }

    /// A binary message.
    pub fn binary(data: impl Into<Bytes>) -> Self {
        Message::Binary(data.into())
    }

    /// A ping message.
    pub fn ping(payload: impl Into<Bytes>) -> Self {
        Message::Ping(payload.into())
    }

    /// A pong message.
    pub fn pong(payload: impl Into<Bytes>) -> Self {
        Message::Pong(payload.into())
    }

    /// A close message with a code and reason.
    pub fn close(code: u16, reason: impl Into<String>) -> Self {
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))
    }

    /// A text message containing `value` as JSON.
    #[cfg(feature = "json")]
    pub fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<Self, Error> {
        serde_json::to_string(value)
            .map(Message::Text)
            .map_err(|e| Error::internal("failed to serialize WebSocket message").with_source(e))
    }

    /// Parse a text or binary message as JSON (`422` error on failure).
    #[cfg(feature = "json")]
    pub fn parse_json<T: serde::de::DeserializeOwned>(&self) -> Result<T, Error> {
        serde_json::from_slice(self.as_bytes()).map_err(|e| {
            Error::unprocessable(format!("invalid JSON message: {e}")).with_code("ws_invalid_json")
        })
    }

    /// The text, if this is a text message.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The text, if this is a text message.
    pub fn into_text(self) -> Option<String> {
        match self {
            Message::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The raw payload of any message (the reason for close frames).
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Message::Text(t) => t.as_bytes(),
            Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b,
            Message::Close(Some(f)) => f.reason.as_bytes(),
            Message::Close(None) => &[],
        }
    }

    /// The raw payload of any message (the reason for close frames).
    pub fn into_bytes(self) -> Bytes {
        match self {
            Message::Text(t) => Bytes::from(t),
            Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b,
            Message::Close(Some(f)) => Bytes::from(f.reason),
            Message::Close(None) => Bytes::new(),
        }
    }

    /// Whether this is a text message.
    pub fn is_text(&self) -> bool {
        matches!(self, Message::Text(_))
    }
    /// Whether this is a binary message.
    pub fn is_binary(&self) -> bool {
        matches!(self, Message::Binary(_))
    }
    /// Whether this is a ping.
    pub fn is_ping(&self) -> bool {
        matches!(self, Message::Ping(_))
    }
    /// Whether this is a pong.
    pub fn is_pong(&self) -> bool {
        matches!(self, Message::Pong(_))
    }
    /// Whether this is a close frame.
    pub fn is_close(&self) -> bool {
        matches!(self, Message::Close(_))
    }

    fn from_tungstenite(msg: ts::Message) -> Option<Self> {
        Some(match msg {
            ts::Message::Text(t) => {
                let bytes = Vec::from(Bytes::from(t));
                Message::Text(
                    String::from_utf8(bytes)
                        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
                )
            }
            ts::Message::Binary(b) => Message::Binary(b),
            ts::Message::Ping(b) => Message::Ping(b),
            ts::Message::Pong(b) => Message::Pong(b),
            ts::Message::Close(f) => Message::Close(f.map(|f| CloseFrame {
                code: f.code.into(),
                reason: f.reason.as_str().to_owned(),
            })),
            // Raw frames are never produced when reading.
            ts::Message::Frame(_) => return None,
        })
    }

    fn into_tungstenite(self) -> ts::Message {
        match self {
            Message::Text(t) => ts::Message::Text(t.into()),
            Message::Binary(b) => ts::Message::Binary(b),
            Message::Ping(b) => ts::Message::Ping(b),
            Message::Pong(b) => ts::Message::Pong(b),
            Message::Close(f) => ts::Message::Close(f.map(|mut f| {
                truncate(&mut f.reason, 123);
                ts::protocol::CloseFrame {
                    code: f.code.into(),
                    reason: f.reason.into(),
                }
            })),
        }
    }
}

fn truncate(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

impl From<String> for Message {
    fn from(t: String) -> Self {
        Message::Text(t)
    }
}
impl From<&str> for Message {
    fn from(t: &str) -> Self {
        Message::Text(t.to_owned())
    }
}
impl From<Bytes> for Message {
    fn from(b: Bytes) -> Self {
        Message::Binary(b)
    }
}
impl From<Vec<u8>> for Message {
    fn from(b: Vec<u8>) -> Self {
        Message::Binary(b.into())
    }
}
impl From<&[u8]> for Message {
    fn from(b: &[u8]) -> Self {
        Message::Binary(Bytes::copy_from_slice(b))
    }
}
