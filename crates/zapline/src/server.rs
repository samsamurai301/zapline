//! Thread-per-core HTTP/1.1 server.
//!
//! Each worker thread owns a single-threaded Tokio runtime and its own
//! `SO_REUSEPORT` listener, so the kernel spreads connections across cores and
//! a connection never migrates between threads (no work-stealing overhead, no
//! cross-core cache traffic on the hot path).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::app::{AppInner, ResponseFuture};
use crate::body::ReqBody;
use crate::request::ConnInfo;

/// Server tuning knobs. The defaults are what the benchmarks use.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ServerConfig {
    /// Worker threads, each with its own runtime and `SO_REUSEPORT` listener.
    /// Defaults to the number of available CPUs.
    pub workers: Option<usize>,
    /// Abort connections that do not send complete headers in time
    /// (slowloris protection). `None` disables it.
    pub header_read_timeout: Option<Duration>,
    /// TCP listen backlog.
    pub backlog: i32,
    /// Print the listening address on startup.
    pub banner: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            workers: None,
            header_read_timeout: Some(Duration::from_secs(30)),
            backlog: 4096,
            banner: true,
        }
    }
}

#[derive(Clone)]
struct Service {
    app: Arc<AppInner>,
    conn: ConnInfo,
}

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for Service {
    type Response = crate::Response;
    type Error = std::convert::Infallible;
    type Future = ResponseFuture;

    #[inline]
    fn call(&self, req: http::Request<hyper::body::Incoming>) -> ResponseFuture {
        self.app.handle(req.map(ReqBody::Incoming), self.conn)
    }
}

fn bind(addr: SocketAddr, backlog: i32, reuse_port: bool) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(
        unix,
        not(any(target_os = "solaris", target_os = "illumos", target_os = "cygwin"))
    ))]
    if reuse_port {
        socket.set_reuse_port(true)?;
    }
    #[cfg(not(unix))]
    let _ = reuse_port;
    socket.set_tcp_nodelay(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(backlog)?;
    Ok(socket.into())
}

pub(crate) fn run(app: AppInner, addr: SocketAddr, config: ServerConfig) -> io::Result<()> {
    let workers = config
        .workers
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let config = Arc::new(config);

    // SO_REUSEPORT load-balances across listeners only on Linux/FreeBSD/etc.
    // Elsewhere, fall back to one listener on a multi-threaded runtime.
    let per_core = cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd"
    ));

    let first = bind(addr, config.backlog, per_core)?;
    let local = first.local_addr()?;
    if config.banner {
        let mode = if per_core {
            "thread-per-core"
        } else {
            "multi-threaded"
        };
        eprintln!("zapline listening on http://{local} ({workers} workers, {mode})");
    }

    if !per_core {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()?;
        let app = Arc::new(app);
        return rt.block_on(async move { serve(app, TcpListener::from_std(first)?, config).await });
    }

    let mut listeners = vec![first];
    for _ in 1..workers {
        // Reuse the resolved port so `:0` binds every worker to the same one.
        listeners.push(bind(local, config.backlog, true)?);
    }

    let handles: Vec<_> = listeners
        .into_iter()
        .enumerate()
        .map(|(i, std_listener)| {
            let app = Arc::new(app.for_worker());
            let config = config.clone();
            std::thread::Builder::new()
                .name(format!("zapline-worker-{i}"))
                .spawn(move || -> io::Result<()> {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    rt.block_on(async move {
                        serve(app, TcpListener::from_std(std_listener)?, config).await
                    })
                })
        })
        .collect::<io::Result<_>>()?;

    for h in handles {
        match h.join() {
            Ok(r) => r?,
            Err(_) => return Err(io::Error::other("worker thread panicked")),
        }
    }
    Ok(())
}

pub(crate) async fn serve(
    app: Arc<AppInner>,
    listener: TcpListener,
    config: Arc<ServerConfig>,
) -> io::Result<()> {
    let local_addr = listener.local_addr().ok();
    loop {
        let (stream, remote_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) if is_transient(&e) => {
                // e.g. EMFILE: back off instead of spinning.
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        let _ = stream.set_nodelay(true);
        let service = Service {
            app: app.clone(),
            conn: ConnInfo {
                remote_addr: Some(remote_addr),
                local_addr,
                secure: false,
            },
        };
        let header_timeout = config.header_read_timeout;
        tokio::spawn(async move {
            let mut builder = http1::Builder::new();
            builder.keep_alive(true).pipeline_flush(true);
            if let Some(t) = header_timeout {
                builder.timer(TokioTimer::new()).header_read_timeout(t);
            } else {
                builder.header_read_timeout(None);
            }
            let conn = builder.serve_connection(TokioIo::new(stream), service);
            #[cfg(feature = "ws")]
            let conn = conn.with_upgrades();
            let _ = conn.await;
        });
    }
}

fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    ) || e.raw_os_error().is_some_and(|c| c == 24 || c == 23) // EMFILE / ENFILE
}
