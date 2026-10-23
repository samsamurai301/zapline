//! HTTP/3 (experimental): a quinn endpoint whose requests go through
//! `AppInner::handle`, like HTTP/1.1 and HTTP/2 ones.
//!
//! Each QUIC connection is a task that accepts request streams; each
//! request is a task of its own. The h3 request stream is split: the
//! receive half becomes the request body ([`RecvBody`]), the send half
//! carries the response head, data frames and trailers.
//!
//! Shutdown follows the TCP server: draining sends GOAWAY (in-flight
//! requests finish, new ones are refused) and stops accepting connections;
//! forcing closes the connections.

use std::future::{poll_fn, Future};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use h3::error::Code;
use h3::server::{RequestResolver, RequestStream};
use http::header::{CONNECTION, CONTENT_LENGTH, TRANSFER_ENCODING, UPGRADE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body::{Body as _, Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::shutdown::{Signal, Slot, DRAINING, RUNNING};
use super::ServerConfig;
use crate::app::AppInner;
use crate::body::{BoxError, ReqBody};
use crate::http3::{AltSvcSlot, Http3Config};
use crate::request::ConnInfo;
use crate::response::Response;
use crate::tls::TlsConfig;

/// `H3_NO_ERROR`, for closing connections cleanly.
const NO_ERROR: u32 = 0x100;

/// How long a drained connection waits for the client to close it after
/// the last response, so the final bytes are delivered before we close.
const LINGER: Duration = Duration::from_secs(2);

/// A bound UDP socket plus everything needed to serve HTTP/3 on it.
pub(crate) struct H3Listener {
    socket: std::net::UdpSocket,
    quic: quinn::ServerConfig,
    settings: Settings,
}

#[derive(Clone)]
struct Settings {
    max_field_section_size: Option<u64>,
    handshake_timeout: Duration,
    header_read_timeout: Option<Duration>,
    max_connections: Option<usize>,
    shutdown_timeout: Duration,
}

impl H3Listener {
    /// The UDP address HTTP/3 is served on.
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// Bind the UDP socket when HTTP/3 is configured: on the TCP listener's IP,
/// at the configured port or else the TCP one. Publishes the `Alt-Svc`
/// value once the port is known.
pub(crate) fn bind(
    config: &ServerConfig,
    tcp: SocketAddr,
    alt_svc: Option<&AltSvcSlot>,
) -> io::Result<Option<H3Listener>> {
    let Some(h3) = &config.http3 else {
        return Ok(None);
    };
    let Some(tls) = &config.tls else {
        return Err(super::invalid_input(
            "HTTP/3 requires TLS: call App::tls(..)",
        ));
    };
    let quic = quic_config(tls, h3)?;
    let addr = SocketAddr::new(tcp.ip(), h3.port.unwrap_or(tcp.port()));
    let socket = std::net::UdpSocket::bind(addr).map_err(|e| {
        io::Error::new(e.kind(), format!("cannot bind HTTP/3 (UDP) on {addr}: {e}"))
    })?;
    socket.set_nonblocking(true)?;
    if let Some(slot) = alt_svc {
        if let Some(value) = h3.alt_svc(socket.local_addr()?.port()) {
            let _ = slot.set(value);
        }
    }
    Ok(Some(H3Listener {
        socket,
        quic,
        settings: Settings {
            max_field_section_size: config.max_header_size.map(|n| n as u64),
            handshake_timeout: config.tls_handshake_timeout,
            header_read_timeout: config.header_read_timeout,
            max_connections: config.max_connections,
            shutdown_timeout: config.shutdown_timeout,
        },
    }))
}

/// The QUIC server configuration: the app's certificate, ALPN `h3`, and the
/// transport limits from [`Http3Config`].
fn quic_config(tls: &TlsConfig, h3: &Http3Config) -> io::Result<quinn::ServerConfig> {
    let mut rustls = (**tls.rustls_config()).clone();
    rustls.alpn_protocols = vec![b"h3".to_vec()];
    // No 0-RTT: early data can be replayed.
    rustls.max_early_data_size = 0;
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls).map_err(|e| {
        super::invalid_input(&format!(
            "HTTP/3 needs TLS 1.3 with TLS13_AES_128_GCM_SHA256 in the TLS config: {e}"
        ))
    })?;
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(h3.max_concurrent_streams.into());
    let idle = quinn::IdleTimeout::try_from(h3.max_idle_timeout)
        .map_err(|_| super::invalid_input("Http3Config::max_idle_timeout is too large"))?;
    transport.max_idle_timeout(Some(idle));
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    server.transport_config(Arc::new(transport));
    Ok(server)
}

/// Accept QUIC connections until shutdown, then drain them. Must run inside
/// a Tokio runtime (quinn drives the endpoint from it).
pub(crate) async fn serve(
    listener: H3Listener,
    app: Arc<AppInner>,
    signal: Signal,
) -> io::Result<()> {
    let H3Listener {
        socket,
        quic,
        settings,
    } = listener;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(quic),
        socket,
        Arc::new(quinn::TokioRuntime),
    )?;
    let local_addr = endpoint.local_addr().ok();
    let limit = settings
        .max_connections
        .map(|n| Arc::new(Semaphore::new(n)));
    let settings = Arc::new(settings);
    let mut stop = signal.register();

    loop {
        let incoming = tokio::select! {
            biased;
            () = stop.stopped() => break,
            i = endpoint.accept() => i,
        };
        let Some(incoming) = incoming else { break };
        let permit = match &limit {
            None => None,
            Some(sem) => match sem.clone().try_acquire_owned() {
                Ok(p) => Some(p),
                // QUIC has no kernel backlog to queue in: refuse right away
                // so the client can fall back to TCP.
                Err(_) => {
                    incoming.refuse();
                    continue;
                }
            },
        };
        let conn = Conn {
            app: app.clone(),
            local_addr,
            settings: settings.clone(),
        };
        tokio::spawn(conn.run(incoming, signal.register(), permit));
    }

    // Refuse new connections, let the open ones finish, then close.
    drop(stop);
    endpoint.set_server_config(None);
    signal.drain(settings.shutdown_timeout).await;
    endpoint.close(NO_ERROR.into(), b"server shutting down");
    let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    Ok(())
}

/// What one connection task needs.
struct Conn {
    app: Arc<AppInner>,
    local_addr: Option<SocketAddr>,
    settings: Arc<Settings>,
}

impl Conn {
    async fn run(
        self,
        incoming: quinn::Incoming,
        mut slot: Slot,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        let _permit = permit;
        let Ok(connecting) = incoming.accept() else {
            return;
        };
        let handshake = tokio::time::timeout(self.settings.handshake_timeout, connecting);
        let quic = tokio::select! {
            biased;
            () = slot.stopped() => return,
            r = handshake => match r {
                Ok(Ok(conn)) => conn,
                _ => return,
            },
        };
        let info = ConnInfo {
            remote_addr: Some(quic.remote_address()),
            local_addr: self.local_addr,
            secure: true,
        };

        let mut builder = h3::server::builder();
        if let Some(n) = self.settings.max_field_section_size {
            builder.max_field_section_size(n);
        }
        let build = builder.build::<_, Bytes>(h3_quinn::Connection::new(quic.clone()));
        let mut h3 = tokio::select! {
            biased;
            () = slot.stopped() => return,
            r = build => match r {
                Ok(h3) => h3,
                Err(_) => return,
            },
        };

        // Requests in flight on this connection. Dropping the set (the
        // connection closed, or a forced shutdown) cancels them, as hyper
        // does for HTTP/2 streams.
        let mut requests = tokio::task::JoinSet::new();
        let mut state = RUNNING;
        let mut accepting = true;
        let finished = loop {
            // After a GOAWAY (ours or the client's), close once the last
            // request is answered.
            if (state == DRAINING || !accepting) && requests.is_empty() {
                break true;
            }
            tokio::select! {
                biased;
                s = advanced(&mut slot, state) => {
                    state = s;
                    if s != DRAINING {
                        quic.close(NO_ERROR.into(), b"server shutting down");
                        return;
                    }
                    // GOAWAY: requests in flight finish, later ones are
                    // refused.
                    if h3.shutdown(0).await.is_err() {
                        break false;
                    }
                }
                Some(_) = requests.join_next(), if !requests.is_empty() => {}
                // Keeps driving the control stream while draining, too.
                r = h3.accept(), if accepting => match r {
                    Ok(Some(resolver)) => {
                        let timeout = self.settings.header_read_timeout;
                        requests.spawn(request(resolver, self.app.clone(), info, timeout));
                    }
                    Ok(None) => accepting = false,
                    // The client closed, or the connection timed out.
                    Err(_) => break false,
                },
            }
        };
        if finished {
            // Dropping `h3` closes the connection at once, which can cut off
            // the last response bytes still in flight. Give the client a
            // moment to read them and close first.
            tokio::select! {
                biased;
                _ = advanced(&mut slot, DRAINING) => {}
                _ = quic.closed() => {}
                () = tokio::time::sleep(LINGER) => {}
            }
        }
    }
}

/// Resolves with the shutdown state once it moves past `seen`.
fn advanced(slot: &mut Slot, seen: u8) -> impl Future<Output = u8> + '_ {
    poll_fn(move |cx| {
        let state = slot.poll_state(cx);
        if state > seen {
            Poll::Ready(state)
        } else {
            Poll::Pending
        }
    })
}

/// Serve one request stream: hand the request to the app, stream the
/// response back.
async fn request(
    resolver: RequestResolver<h3_quinn::Connection, Bytes>,
    app: Arc<AppInner>,
    conn: ConnInfo,
    header_timeout: Option<Duration>,
) {
    // Like HTTP/1.1's header read timeout: a stream that never sends its
    // headers must not hold a stream slot forever.
    let resolved = match header_timeout {
        Some(t) => match tokio::time::timeout(t, resolver.resolve_request()).await {
            Ok(r) => r,
            Err(_) => return,
        },
        None => resolver.resolve_request().await,
    };
    let Ok((req, stream)) = resolved else {
        return;
    };
    let (mut send, recv) = stream.split();
    let head_only = req.method() == Method::HEAD;
    let length = content_length(req.headers());
    let req = req.map(|()| ReqBody::Stream(Box::pin(RecvBody::new(recv, length))));
    let res = match app.handle(req, conn).await {
        Ok(res) => res,
        Err(never) => match never {},
    };
    if send_response(&mut send, res, head_only).await.is_err() {
        send.stop_stream(Code::H3_INTERNAL_ERROR);
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// Headers that are specific to an HTTP/1.1 connection and malformed in
/// HTTP/3 (RFC 9114, section 4.2).
const CONNECTION_HEADERS: [&str; 2] = ["keep-alive", "proxy-connection"];

type SendStream = RequestStream<h3_quinn::SendStream<Bytes>, Bytes>;

async fn send_response(
    send: &mut SendStream,
    res: Response,
    head_only: bool,
) -> Result<(), BoxError> {
    let (mut parts, mut body) = res.into_parts();
    let headers = &mut parts.headers;
    for name in [CONNECTION, TRANSFER_ENCODING, UPGRADE] {
        headers.remove(name);
    }
    for name in CONNECTION_HEADERS {
        headers.remove(HeaderName::from_static(name));
    }
    let status = parts.status;
    let no_body = status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED;
    if !no_body && !headers.contains_key(CONTENT_LENGTH) {
        if let Some(n) = body.size_hint().exact() {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(n));
        }
    }
    send.send_response(http::Response::from_parts(parts, ()))
        .await?;
    if !(head_only || no_body) {
        while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            let frame = frame?;
            match frame.into_data() {
                Ok(data) => {
                    if !data.is_empty() {
                        send.send_data(data).await?;
                    }
                }
                Err(frame) => {
                    if let Ok(trailers) = frame.into_trailers() {
                        send.send_trailers(trailers).await?;
                        break;
                    }
                }
            }
        }
    }
    send.finish().await?;
    Ok(())
}

/// The receive half of an h3 request stream as an `http_body::Body`.
struct RecvBody {
    stream: RequestStream<h3_quinn::RecvStream, Bytes>,
    length: Option<u64>,
    data_done: bool,
    done: bool,
}

impl RecvBody {
    fn new(stream: RequestStream<h3_quinn::RecvStream, Bytes>, length: Option<u64>) -> Self {
        RecvBody {
            stream,
            length,
            data_done: false,
            done: false,
        }
    }
}

impl http_body::Body for RecvBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        if !this.data_done {
            match ready!(this.stream.poll_recv_data(cx)) {
                Ok(Some(mut buf)) => {
                    let data = buf.copy_to_bytes(buf.remaining());
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Ok(None) => this.data_done = true,
                Err(e) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(Box::new(e))));
                }
            }
        }
        let trailers = ready!(this.stream.poll_recv_trailers(cx));
        this.done = true;
        match trailers {
            Ok(Some(trailers)) => Poll::Ready(Some(Ok(Frame::trailers(trailers)))),
            Ok(None) => Poll::Ready(None),
            Err(e) => Poll::Ready(Some(Err(Box::new(e)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }

    fn size_hint(&self) -> SizeHint {
        match self.length {
            Some(n) if !self.data_done => SizeHint::with_exact(n),
            _ => SizeHint::default(),
        }
    }
}
