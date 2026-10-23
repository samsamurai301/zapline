//! Accept loops and per-connection protocol handling.
//!
//! Codegen matters on this path: hyper's HTTP/1 dispatcher is instantiated
//! once per stream type, and LTO only inlines its parser and encoder into
//! the connection task while there is a single instantiation. So plain TCP
//! is always served on a bare `TcpStream` (HTTP/2 is detected with `peek`),
//! and the Unix-socket and TLS paths are only reachable through function
//! pointers created by `App::run_unix` and `TlsConfig`: binaries that never
//! use them do not contain them, and keep the fast single-instantiation
//! HTTP/1.1 path. For the same reason upgraded (WebSocket) connections are
//! accounted for from the handler (`server::upgrade`), never by wrapping
//! the IO type here.

use std::future::poll_fn;
#[cfg(feature = "http2")]
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use super::limit::{Limit, Permit};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};

#[cfg(feature = "http2")]
use hyper::server::conn::http2;
#[cfg(feature = "http2")]
use hyper_util::rt::TokioExecutor;

#[cfg(any(feature = "tls", feature = "http2"))]
use super::shutdown::Slot;
use super::shutdown::{GracefulShutdown, Signal, Watched};
use super::ServerConfig;
use crate::app::{AppInner, ResponseFuture};
use crate::body::ReqBody;
#[cfg(feature = "tls")]
use crate::handler::BoxFuture;
use crate::request::ConnInfo;

/// A byte stream a connection can be served on.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

/// A stream fresh from a listener.
pub(crate) trait Accepted: Io + Sized {
    /// What HTTP/1.1 or HTTP/2 runs on after protocol detection.
    #[cfg(feature = "http2")]
    type Detected: Io;

    /// Tell HTTP/2 prior knowledge (the client preface) from HTTP/1.1
    /// without losing any bytes.
    #[cfg(feature = "http2")]
    fn detect_h2(self) -> impl Future<Output = io::Result<(Self::Detected, bool)>> + Send;

    /// The TLS entry point for this stream type.
    #[cfg(feature = "tls")]
    fn tls_entry(entry: &TlsEntry) -> TlsServe<Self>;
}

#[cfg(feature = "http2")]
const H2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

impl Accepted for TcpStream {
    #[cfg(feature = "http2")]
    type Detected = TcpStream;

    #[cfg(feature = "http2")]
    async fn detect_h2(self) -> io::Result<(TcpStream, bool)> {
        let mut buf = [0u8; 24];
        let mut wait = Duration::from_millis(2);
        loop {
            let n = self.peek(&mut buf).await?;
            let matches = buf[..n] == H2_PREFACE[..n];
            if !matches || n == 0 || n == H2_PREFACE.len() {
                return Ok((self, matches && n == H2_PREFACE.len()));
            }
            // A partial preface. `peek` does not consume, so it would spin;
            // real h2c clients send the whole preface in one write, so wait
            // briefly with backoff (~60ms total), then let HTTP/1 handle the
            // connection (its header timeout costs no CPU while idle).
            if wait > Duration::from_millis(32) {
                return Ok((self, false));
            }
            tokio::time::sleep(wait).await;
            wait *= 2;
        }
    }

    #[cfg(feature = "tls")]
    fn tls_entry(entry: &TlsEntry) -> TlsServe<Self> {
        entry.tcp
    }
}

#[cfg(unix)]
impl Accepted for tokio::net::UnixStream {
    #[cfg(feature = "http2")]
    type Detected = Rewind<tokio::net::UnixStream>;

    #[cfg(feature = "http2")]
    async fn detect_h2(mut self) -> io::Result<(Self::Detected, bool)> {
        use tokio::io::AsyncReadExt;

        // Unix streams cannot peek: read, then replay what was read. Large
        // enough for most request heads, so HTTP/1.1 needs no extra read.
        let mut buf = bytes::BytesMut::with_capacity(1024);
        loop {
            let n = self.read_buf(&mut buf).await?;
            let k = buf.len().min(H2_PREFACE.len());
            let matches = buf[..k] == H2_PREFACE[..k];
            if !matches || n == 0 || k == H2_PREFACE.len() {
                let is_h2 = matches && k == H2_PREFACE.len();
                return Ok((Rewind::new(self, buf.freeze()), is_h2));
            }
        }
    }

    #[cfg(feature = "tls")]
    fn tls_entry(entry: &TlsEntry) -> TlsServe<Self> {
        entry.unix
    }
}

/// A listening socket.
pub(crate) trait Listen: Send + 'static {
    type Io: Accepted;
    fn poll_accept(&self, cx: &mut Context<'_>)
        -> Poll<io::Result<(Self::Io, Option<SocketAddr>)>>;
    fn local_addr(&self) -> Option<SocketAddr>;
}

impl Listen for TcpListener {
    type Io = TcpStream;

    #[inline]
    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(TcpStream, Option<SocketAddr>)>> {
        TcpListener::poll_accept(self, cx).map_ok(|(stream, addr)| {
            let _ = stream.set_nodelay(true);
            (stream, Some(addr))
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        TcpListener::local_addr(self).ok()
    }
}

#[cfg(unix)]
impl Listen for tokio::net::UnixListener {
    type Io = tokio::net::UnixStream;

    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(tokio::net::UnixStream, Option<SocketAddr>)>> {
        tokio::net::UnixListener::poll_accept(self, cx).map_ok(|(stream, _)| (stream, None))
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        None
    }
}

/// The hyper service for one connection.
#[derive(Clone)]
pub(crate) struct Service {
    app: Arc<AppInner>,
    conn: ConnInfo,
}

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for Service {
    type Response = crate::Response;
    type Error = std::convert::Infallible;
    type Future = ResponseFuture;

    /// HTTP/1: see [`ResponseFuture::poll_now`].
    #[inline]
    fn call(&self, req: http::Request<hyper::body::Incoming>) -> ResponseFuture {
        self.app
            .handle(req.map(ReqBody::Incoming), self.conn)
            .poll_now()
    }
}

/// The hyper service for one HTTP/2 connection. hyper runs every stream as
/// its own task, so the response future is left for that task to poll.
#[cfg(feature = "http2")]
pub(crate) struct H2Service(Service, Arc<Activity>);

#[cfg(feature = "http2")]
impl hyper::service::Service<http::Request<hyper::body::Incoming>> for H2Service {
    type Response = crate::Response;
    type Error = std::convert::Infallible;
    type Future = Tracked;

    fn call(&self, req: http::Request<hyper::body::Incoming>) -> Tracked {
        use std::sync::atomic::Ordering::Relaxed;
        self.1.requests.fetch_add(1, Relaxed);
        self.1.in_flight.fetch_add(1, Relaxed);
        Tracked(
            self.0.app.handle(req.map(ReqBody::Incoming), self.0.conn),
            self.1.clone(),
        )
    }
}

/// Closing idle HTTP/2 connections.
#[cfg(feature = "http2")]
mod idle {
    use super::*;

    /// Requests seen on one HTTP/2 connection, for closing it when idle.
    #[derive(Default)]
    pub(crate) struct Activity {
        pub(super) requests: std::sync::atomic::AtomicU64,
        pub(super) in_flight: std::sync::atomic::AtomicUsize,
    }

    /// A response future that counts as in flight until it finishes or drops.
    pub(crate) struct Tracked(pub(super) ResponseFuture, pub(super) Arc<Activity>);

    impl Future for Tracked {
        type Output = Result<crate::Response, std::convert::Infallible>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.0).poll(cx)
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.1
                .in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pin_project_lite::pin_project! {
        /// An HTTP/2 connection that sends GOAWAY once no request has been in
        /// flight for `idle`: idle clients can't hold sockets (and
        /// `max_connections` slots) forever. Responses still streaming, such as
        /// SSE, finish first.
            pub(crate) struct IdleH2<C> {
            #[pin]
            conn: C,
            #[pin]
            timer: tokio::time::Sleep,
            idle: Duration,
            activity: Arc<Activity>,
            seen: u64,
            closing: bool,
        }
    }

    impl<C> IdleH2<C> {
        pub(crate) fn new(conn: C, idle: Duration, activity: Arc<Activity>) -> Self {
            IdleH2 {
                conn,
                timer: tokio::time::sleep(idle),
                idle,
                activity,
                seen: 0,
                closing: false,
            }
        }
    }

    impl<C: Future + GracefulShutdown> Future for IdleH2<C> {
        type Output = C::Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<C::Output> {
            use std::sync::atomic::Ordering::Relaxed;
            let mut this = self.project();
            while !*this.closing && this.timer.as_mut().poll(cx).is_ready() {
                let requests = this.activity.requests.load(Relaxed);
                if requests == *this.seen && this.activity.in_flight.load(Relaxed) == 0 {
                    *this.closing = true;
                    this.conn.as_mut().graceful_shutdown();
                } else {
                    *this.seen = requests;
                    let next = tokio::time::Instant::now() + *this.idle;
                    this.timer.as_mut().reset(next);
                }
            }
            this.conn.poll(cx)
        }
    }

    impl<C: GracefulShutdown> GracefulShutdown for IdleH2<C> {
        fn graceful_shutdown(self: Pin<&mut Self>) {
            self.project().conn.graceful_shutdown();
        }
    }
}
#[cfg(feature = "http2")]
use idle::{Activity, IdleH2, Tracked};

/// Serves one TLS connection from a stream of type `I`.
#[cfg(feature = "tls")]
pub(crate) type TlsServe<I> = fn(I, Service, Arc<Proto>, Slot, Option<Permit>) -> BoxFuture<()>;

/// The TLS serving code, one entry per stream type. Only `TlsConfig`
/// creates it, so apps without TLS do not contain that code at all.
#[cfg(feature = "tls")]
#[derive(Clone, Copy)]
pub(crate) struct TlsEntry {
    tcp: TlsServe<TcpStream>,
    #[cfg(unix)]
    unix: TlsServe<tokio::net::UnixStream>,
}

#[cfg(feature = "tls")]
impl TlsEntry {
    pub(crate) fn new() -> Self {
        TlsEntry {
            tcp: serve_tls::<TcpStream>,
            #[cfg(unix)]
            unix: serve_tls::<tokio::net::UnixStream>,
        }
    }
}

#[cfg(feature = "tls")]
fn serve_tls<I: Accepted>(
    io: I,
    service: Service,
    proto: Arc<Proto>,
    slot: Slot,
    permit: Option<Permit>,
) -> BoxFuture<()> {
    Box::pin(async move {
        proto.serve_tls(io, service, slot).await;
        drop(permit);
    })
}

/// TLS settings resolved for serving.
#[cfg(feature = "tls")]
#[derive(Clone)]
struct Tls {
    acceptor: tokio_rustls::TlsAcceptor,
    entry: TlsEntry,
    handshake_timeout: Duration,
}

/// Protocol settings, built once from the [`ServerConfig`].
#[derive(Clone)]
pub(crate) struct Proto {
    http1: http1::Builder,
    #[cfg(feature = "http2")]
    http2: Option<http2::Builder<TokioExecutor>>,
    #[cfg(feature = "http2")]
    detect_timeout: Option<Duration>,
    #[cfg(feature = "tls")]
    tls: Option<Tls>,
}

impl Proto {
    pub(crate) fn new(config: &ServerConfig) -> Proto {
        let mut h1 = http1::Builder::new();
        h1.keep_alive(config.keep_alive)
            .pipeline_flush(config.pipeline_flush);
        match config.header_read_timeout {
            Some(t) => h1.timer(TokioTimer::new()).header_read_timeout(t),
            None => h1.header_read_timeout(None),
        };
        if let Some(n) = config.max_buf_size {
            h1.max_buf_size(n);
        }
        if let Some(n) = config.max_header_size {
            h1.max_header_size(n);
        }
        if let Some(n) = config.max_headers {
            h1.max_headers(n);
        }

        #[cfg(feature = "http2")]
        let http2 = config.http2.then(|| {
            let mut h2 = http2::Builder::new(TokioExecutor::new());
            h2.timer(TokioTimer::new())
                .keep_alive_interval(config.http2_keep_alive_interval);
            // `None` would mean unlimited; keep hyper's default (200) instead.
            if let Some(n) = config.http2_max_concurrent_streams {
                h2.max_concurrent_streams(n);
            }
            h2
        });

        Proto {
            http1: h1,
            #[cfg(feature = "http2")]
            http2,
            #[cfg(feature = "http2")]
            detect_timeout: config.header_read_timeout,
            #[cfg(feature = "tls")]
            tls: config.tls.as_ref().map(|t| Tls {
                acceptor: t.acceptor(config.http2),
                entry: t.entry(),
                handshake_timeout: config.tls_handshake_timeout,
            }),
        }
    }

    fn is_tls(&self) -> bool {
        #[cfg(feature = "tls")]
        return self.tls.is_some();
        #[cfg(not(feature = "tls"))]
        false
    }

    /// An HTTP/1.1 connection future.
    #[inline]
    fn h1<I: Io>(&self, io: I, service: Service) -> H1<I> {
        let conn = self.http1.serve_connection(TokioIo::new(io), service);
        #[cfg(feature = "ws")]
        let conn = conn.with_upgrades();
        conn
    }

    /// Handshake (inside the connection task, never in the accept loop),
    /// then serve HTTP/2 if ALPN chose `h2`, else HTTP/1.1.
    #[cfg(feature = "tls")]
    async fn serve_tls<I: Io>(&self, io: I, service: Service, mut slot: Slot) {
        let Some(tls) = &self.tls else { return };
        let handshake = tokio::time::timeout(tls.handshake_timeout, tls.acceptor.accept(io));
        let stream = tokio::select! {
            biased;
            () = slot.stopped() => return,
            r = handshake => match r {
                Ok(Ok(stream)) => stream,
                _ => return,
            },
        };
        #[cfg(feature = "http2")]
        if let Some(h2) = &self.http2 {
            if stream.get_ref().1.alpn_protocol() == Some(b"h2") {
                return serve_h2(stream, service, h2, self.detect_timeout, slot).await;
            }
        }
        Watched::new(self.h1(stream, service), slot, None).await
    }

    /// Detect HTTP/2 prior knowledge, then serve the right protocol.
    #[cfg(feature = "http2")]
    async fn serve_detected<I: Accepted>(
        &self,
        io: I,
        service: Service,
        mut slot: Slot,
        permit: Option<Permit>,
    ) {
        let Some(h2) = &self.http2 else { return };
        let detect = within(self.detect_timeout, io.detect_h2());
        let (io, is_h2) = tokio::select! {
            biased;
            () = slot.stopped() => return,
            r = detect => match r {
                Some(Ok(v)) => v,
                _ => return,
            },
        };
        if is_h2 {
            serve_h2(io, service, h2, self.detect_timeout, slot).await;
            drop(permit);
        } else {
            // Its own task, so HTTP/1.1 runs exactly the hot-path future.
            tokio::spawn(Watched::new(self.h1(io, service), slot, permit));
        }
    }
}

#[cfg(not(feature = "ws"))]
type H1<I> = http1::Connection<TokioIo<I>, Service>;
#[cfg(feature = "ws")]
type H1<I> = http1::UpgradeableConnection<TokioIo<I>, Service>;

/// Serve HTTP/2, closing the connection after `idle` without requests
/// (`ServerConfig::header_read_timeout`, as HTTP/1.1 does between requests).
#[cfg(feature = "http2")]
async fn serve_h2<I: Io>(
    io: I,
    service: Service,
    h2: &http2::Builder<TokioExecutor>,
    idle: Option<Duration>,
    slot: Slot,
) {
    let activity = Arc::new(Activity::default());
    let conn = h2.serve_connection(TokioIo::new(io), H2Service(service, activity.clone()));
    match idle {
        Some(idle) => {
            let conn = IdleH2::new(conn, idle, activity);
            Watched::new(conn, slot, None).await
        }
        None => Watched::new(conn, slot, None).await,
    }
}

#[cfg(not(feature = "ws"))]
impl<I: Io> GracefulShutdown for http1::Connection<TokioIo<I>, Service> {
    fn graceful_shutdown(self: Pin<&mut Self>) {
        http1::Connection::graceful_shutdown(self)
    }
}

#[cfg(feature = "ws")]
impl<I: Io> GracefulShutdown for http1::UpgradeableConnection<TokioIo<I>, Service> {
    fn graceful_shutdown(self: Pin<&mut Self>) {
        http1::UpgradeableConnection::graceful_shutdown(self)
    }
}

#[cfg(feature = "http2")]
impl<I: Io> GracefulShutdown for http2::Connection<TokioIo<I>, H2Service, TokioExecutor> {
    fn graceful_shutdown(self: Pin<&mut Self>) {
        http2::Connection::graceful_shutdown(self)
    }
}

/// Everything one accept loop needs.
pub(crate) struct Worker {
    app: Arc<AppInner>,
    proto: Arc<Proto>,
    signal: Signal,
    limit: Option<Arc<Limit>>,
    shutdown_timeout: Duration,
}

impl Worker {
    /// Serves a per-worker copy of `app` (see [`AppInner::for_worker`]).
    /// With the `ws` feature that copy also carries this accept loop's
    /// upgrade accounting.
    pub(crate) fn new(
        app: &AppInner,
        proto: &Proto,
        config: &ServerConfig,
        signal: Signal,
    ) -> Worker {
        let limit = config.max_connections.map(Limit::new);
        #[cfg(feature = "ws")]
        let upgrades =
            super::upgrade::Upgrades::new(signal.clone(), limit.clone(), config.max_upgraded);
        Worker {
            app: Arc::new(app.for_worker(
                #[cfg(feature = "ws")]
                Some(upgrades),
            )),
            // Each worker gets its own copy so per-connection refcounting
            // stays core-local.
            proto: Arc::new(proto.clone()),
            signal,
            limit,
            shutdown_timeout: config.shutdown_timeout,
        }
    }
}

/// Accept connections until shutdown, then drain them.
pub(crate) async fn serve<L: Listen>(listener: L, worker: Worker) -> io::Result<()> {
    let local_addr = listener.local_addr();
    let secure = worker.proto.is_tls();
    let mut stop = worker.signal.register();

    let result = loop {
        let permit = match &worker.limit {
            None => None,
            Some(limit) => tokio::select! {
                biased;
                () = stop.stopped() => break Ok(()),
                p = limit.acquire() => Some(p),
            },
        };
        let accepted = tokio::select! {
            biased;
            () = stop.stopped() => break Ok(()),
            r = poll_fn(|cx| listener.poll_accept(cx)) => r,
        };
        match accepted {
            Ok((io, remote_addr)) => {
                let conn = ConnInfo {
                    remote_addr,
                    local_addr,
                    secure,
                };
                spawn_connection(io, conn, &worker, permit);
            }
            // e.g. EMFILE/ENOMEM: back off instead of spinning or exiting.
            Err(e) if is_transient(&e) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => break Err(e),
        }
    };

    // Stop accepting: close the listening socket, then wait for connections.
    drop(stop);
    drop(listener);
    worker.signal.drain(worker.shutdown_timeout).await;
    result
}

fn spawn_connection<I: Accepted>(io: I, conn: ConnInfo, worker: &Worker, permit: Option<Permit>) {
    let slot = worker.signal.register();
    let service = Service {
        app: worker.app.clone(),
        conn,
    };

    #[cfg(feature = "tls")]
    if let Some(tls) = &worker.proto.tls {
        let serve = I::tls_entry(&tls.entry);
        tokio::spawn(serve(io, service, worker.proto.clone(), slot, permit));
        return;
    }

    #[cfg(feature = "http2")]
    if worker.proto.http2.is_some() {
        let proto = worker.proto.clone();
        tokio::spawn(async move { proto.serve_detected(io, service, slot, permit).await });
        return;
    }

    // Plain HTTP/1.1, the hot path: the task is just the watched connection.
    tokio::spawn(Watched::new(worker.proto.h1(io, service), slot, permit));
}

/// Accept errors are per-connection or resource exhaustion (EMFILE, ENFILE,
/// ENOMEM, ENOBUFS, ECONNABORTED, EPROTO, EPERM, ...) and worth riding out
/// with backoff, except the few that mean the listener itself is unusable.
fn is_transient(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        // EBADF, EINVAL, ENOTSOCK, EOPNOTSUPP (Linux numbering; other unixes
        // report these via ErrorKind below as well).
        if let Some(code) = e.raw_os_error() {
            if cfg!(any(target_os = "linux", target_os = "android")) {
                return !matches!(code, 9 | 22 | 88 | 95);
            }
        }
    }
    !matches!(
        e.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported | io::ErrorKind::NotConnected
    )
}

/// `f`, bounded by `timeout` when there is one (`None` on timeout).
#[cfg(feature = "http2")]
async fn within<F: Future>(timeout: Option<Duration>, f: F) -> Option<F::Output> {
    match timeout {
        Some(t) => tokio::time::timeout(t, f).await.ok(),
        None => Some(f.await),
    }
}

/// An IO that first yields bytes that were already read from it.
#[cfg(all(unix, feature = "http2"))]
pub(crate) struct Rewind<I> {
    prefix: Option<bytes::Bytes>,
    io: I,
}

#[cfg(all(unix, feature = "http2"))]
impl<I> Rewind<I> {
    fn new(io: I, prefix: bytes::Bytes) -> Self {
        Rewind {
            prefix: (!prefix.is_empty()).then_some(prefix),
            io,
        }
    }
}

#[cfg(all(unix, feature = "http2"))]
impl<I: AsyncRead + Unpin> AsyncRead for Rewind<I> {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(mut prefix) = self.prefix.take() else {
            return Pin::new(&mut self.io).poll_read(cx, buf);
        };
        let n = prefix.len().min(buf.remaining());
        buf.put_slice(&prefix.split_to(n));
        if !prefix.is_empty() {
            self.prefix = Some(prefix);
        } else if buf.remaining() > 0 {
            // Top up with whatever the socket already has, so this read sees
            // as much as a plain socket read would. Errors and EOF resurface
            // on the next read.
            let _ = Pin::new(&mut self.io).poll_read(cx, buf);
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(all(unix, feature = "http2"))]
impl<I: AsyncWrite + Unpin> AsyncWrite for Rewind<I> {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    #[inline]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    #[inline]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
