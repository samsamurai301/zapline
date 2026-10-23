//! The HTTP server: thread-per-core runtimes, HTTP/1.1 and HTTP/2, TLS,
//! graceful shutdown and connection limits.
//!
//! [`App::run`] is all most apps need. It serves until SIGINT or SIGTERM
//! (Ctrl-C on Windows), then stops accepting, lets in-flight requests finish
//! (for up to [`ServerConfig::shutdown_timeout`]), closes the remaining
//! connections and runs the shutdown hooks. A second signal skips the wait.
//! Upgraded WebSockets are part of the drain (they get a `1001` close
//! frame) and of [`ServerConfig::max_connections`].
//!
//! ```no_run
//! use std::time::Duration;
//! use zapline::prelude::*;
//!
//! fn main() -> std::io::Result<()> {
//!     App::new()
//!         .get("/", || async { "hi" })
//!         .shutdown_timeout(Duration::from_secs(10))
//!         .max_connections(10_000)
//!         .run("0.0.0.0:8080")
//! }
//! ```
//!
//! By default ([`RuntimeMode::PerCore`]) each worker thread owns a
//! single-threaded Tokio runtime and its own `SO_REUSEPORT` listener, so the
//! kernel spreads connections across cores and a connection never migrates
//! between threads (no work-stealing overhead, no cross-core cache traffic on
//! the hot path).
//!
//! Under systemd (`Type=notify`) the server reports `READY=1` once it is
//! listening, after an optional in-process health check; see
//! [`ENV_READY_PATH`] for zero-downtime swaps.

mod conn;
mod limit;
mod ready;
mod shutdown;
#[cfg(feature = "ws")]
mod upgrade;

pub use self::ready::{ENV_READY_PATH, ENV_READY_TIMEOUT, ENV_REUSE_PORT};

use std::future::Future;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::app::{App, AppInner, Prepared, Running};
use crate::handler::BoxFuture;

#[cfg(feature = "tls")]
pub(crate) use self::conn::TlsEntry;
use self::conn::{Proto, Worker};
use self::shutdown::{Signal, Trigger};
#[cfg(feature = "ws")]
pub(crate) use self::upgrade::{Refused, ShutdownWatch, Upgrades};

/// Whether this platform load-balances `SO_REUSEPORT` listeners.
const REUSEPORT_BALANCES: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd"
));

/// How worker threads and connections are scheduled.
///
/// ```no_run
/// use zapline::prelude::*;
/// use zapline::server::RuntimeMode;
///
/// # fn main() -> std::io::Result<()> {
/// App::new()
///     .get("/", || async { "hi" })
///     .runtime(RuntimeMode::MultiThread)
///     .run("0.0.0.0:8080")
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeMode {
    /// One single-threaded runtime and one `SO_REUSEPORT` listener per
    /// worker; connections never move between threads. The fastest mode for
    /// typical request/response work, and the default on Linux, Android and
    /// FreeBSD. Elsewhere (no `SO_REUSEPORT` load balancing) TCP servers fall
    /// back to [`MultiThread`](Self::MultiThread).
    PerCore,
    /// One listener on a shared work-stealing runtime with `workers` threads.
    /// Better when load per connection is very uneven, or when handlers share
    /// heavy I/O resources (connection pools, long-lived clients) that should
    /// be driven from any thread.
    MultiThread,
}

impl Default for RuntimeMode {
    fn default() -> Self {
        if REUSEPORT_BALANCES {
            RuntimeMode::PerCore
        } else {
            RuntimeMode::MultiThread
        }
    }
}

/// Server tuning knobs. The defaults are production-ready and are what the
/// benchmarks use.
///
/// Set fields through [`App::server_config_mut`] or the matching `App`
/// builder methods ([`App::workers`], [`App::runtime`], [`App::http2`],
/// [`App::max_connections`], [`App::shutdown_timeout`], `App::tls`):
///
/// ```
/// use std::time::Duration;
/// use zapline::prelude::*;
///
/// let mut app = App::new().max_connections(5_000);
/// let config = app.server_config_mut();
/// config.keep_alive = false;
/// config.max_header_size = Some(16 * 1024);
/// config.header_read_timeout = Some(Duration::from_secs(10));
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ServerConfig {
    /// Worker threads. Defaults to the number of available CPUs.
    pub workers: Option<usize>,
    /// How workers schedule connections (default: [`RuntimeMode::PerCore`]
    /// where supported). Ignored by [`App::serve`], which uses your runtime.
    pub runtime: RuntimeMode,
    /// Abort connections that do not send complete request headers in time
    /// (slowloris protection); also bounds HTTP/2 detection, and closes
    /// HTTP/2 connections that send no request for this long. `None`
    /// disables it. Default: 30s.
    pub header_read_timeout: Option<Duration>,
    /// TCP listen backlog. Default: 4096.
    pub backlog: i32,
    /// Print one startup line (address, workers, runtime, protocols) and a
    /// line when shutdown starts. Default: `true`. [`App::serve`] never
    /// prints the startup line.
    pub banner: bool,
    /// HTTP/1.1 keep-alive. Default: `true`.
    pub keep_alive: bool,
    /// Batch responses to pipelined HTTP/1.1 requests into fewer writes, a
    /// large win for pipelining clients. Trade-off (a hyper 1.12 bug): a
    /// malformed or oversized request head then closes the connection
    /// without the automatic `400`/`414`/`431` response, and responses to
    /// earlier pipelined requests still buffered on that connection are lost
    /// too. The guide's "Upstream issues" page has the details. Default:
    /// `true`.
    pub pipeline_flush: bool,
    /// Maximum open connections per accept loop: per worker in
    /// [`RuntimeMode::PerCore`], in total otherwise. At the limit the server
    /// stops accepting until a connection closes (the kernel backlog queues
    /// new ones). Upgraded (WebSocket) connections keep counting after the
    /// upgrade, unless `max_upgraded` gives them their own limit. Default:
    /// unlimited.
    pub max_connections: Option<usize>,
    /// Count upgraded (WebSocket) connections against this separate limit,
    /// per accept loop like `max_connections`, instead of against
    /// `max_connections`. An upgrade beyond it is refused with `503`
    /// (`ws_too_many_connections`) before the handshake, and the HTTP
    /// connection's `max_connections` slot is freed at the upgrade. `None`
    /// (default): upgraded connections share `max_connections`.
    ///
    /// ```
    /// use zapline::prelude::*;
    ///
    /// let mut app = App::new().max_connections(1_000);
    /// // Up to 1,000 HTTP connections and, separately, 50,000 WebSockets.
    /// app.server_config_mut().max_upgraded = Some(50_000);
    /// ```
    pub max_upgraded: Option<usize>,
    /// Maximum size in bytes of an HTTP/1.1 request head (request line and
    /// headers); larger heads are rejected (see `pipeline_flush`). `None`
    /// (default): bounded by `max_buf_size`.
    pub max_header_size: Option<usize>,
    /// Maximum number of HTTP/1.1 request headers. `None` (default): 100.
    /// Setting it moves header parsing to the heap (about 5% slower).
    pub max_headers: Option<usize>,
    /// Maximum HTTP/1.1 read buffer per connection, at least 8192 bytes.
    /// `None` (default): about 400 KiB.
    pub max_buf_size: Option<usize>,
    /// Serve HTTP/2 next to HTTP/1.1: h2c with prior knowledge on plain
    /// connections and ALPN `h2` over TLS. `false` means HTTP/1.1 only.
    /// Default: `true` when the `http2` feature is enabled.
    pub http2: bool,
    /// Maximum concurrent streams per HTTP/2 connection. `None`: 200.
    pub http2_max_concurrent_streams: Option<u32>,
    /// Send HTTP/2 keep-alive pings at this interval (and close connections
    /// that stop answering). Default: disabled.
    pub http2_keep_alive_interval: Option<Duration>,
    /// On shutdown, how long to wait for in-flight requests before closing
    /// the remaining connections. Default: 30s.
    pub shutdown_timeout: Duration,
    /// On shutdown, how long an idle HTTP/1.1 connection may still send a
    /// request (within `shutdown_timeout`). The listening socket closes at
    /// once; this is for requests already on their way, from clients that
    /// just connected or reuse a keep-alive connection: they get a response
    /// (saying `connection: close`) instead of a reset, so a rolling restart
    /// drops nothing. A connection closes right after that response, or
    /// when the grace ends if it sends nothing. `Duration::ZERO` closes idle
    /// connections at once. Default: 1s.
    pub drain_grace: Duration,
    /// Abort TLS handshakes that take longer than this. Default: 10s.
    pub tls_handshake_timeout: Duration,
    /// Terminate TLS on every connection (see [`crate::tls`]).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            workers: None,
            runtime: RuntimeMode::default(),
            header_read_timeout: Some(Duration::from_secs(30)),
            backlog: 4096,
            banner: true,
            keep_alive: true,
            pipeline_flush: true,
            max_connections: None,
            max_upgraded: None,
            max_header_size: None,
            max_headers: None,
            max_buf_size: None,
            http2: cfg!(feature = "http2"),
            http2_max_concurrent_streams: None,
            http2_keep_alive_interval: None,
            shutdown_timeout: Duration::from_secs(30),
            drain_grace: Duration::from_secs(1),
            tls_handshake_timeout: Duration::from_secs(10),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

impl ServerConfig {
    fn is_tls(&self) -> bool {
        #[cfg(feature = "tls")]
        return self.tls.is_some();
        #[cfg(not(feature = "tls"))]
        false
    }

    fn worker_count(&self) -> usize {
        self.workers
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
            .max(1)
    }

    fn protocols(&self) -> &'static str {
        match (self.http2, self.is_tls()) {
            (true, true) => "HTTP/1.1 + HTTP/2, TLS",
            (true, false) => "HTTP/1.1 + h2c",
            (false, true) => "HTTP/1.1, TLS",
            (false, false) => "HTTP/1.1",
        }
    }
}

/// Reject configurations that would fail (or panic) later.
pub(crate) fn check_config(config: &ServerConfig) -> io::Result<()> {
    let invalid = |msg: &str| Err(io::Error::new(io::ErrorKind::InvalidInput, msg.to_owned()));
    if config.max_buf_size.is_some_and(|n| n < 8192) {
        return invalid("ServerConfig::max_buf_size must be at least 8192 bytes");
    }
    if config.max_connections == Some(0) {
        return invalid("ServerConfig::max_connections must be at least 1");
    }
    if config.max_upgraded == Some(0) {
        return invalid("ServerConfig::max_upgraded must be at least 1");
    }
    if config.http2 && !cfg!(feature = "http2") {
        return invalid(&crate::codes::with_help(
            "ZL0020",
            Some("http2"),
            "HTTP/2 requires zapline's `http2` feature",
        ));
    }
    if config.http2_max_concurrent_streams == Some(0) {
        return invalid("ServerConfig::http2_max_concurrent_streams must be at least 1");
    }
    Ok(())
}

/// Resolves on SIGINT or SIGTERM (Ctrl-C on non-unix platforms). This is
/// what [`App::run`] and [`App::serve`] stop on; combine it with your own
/// signals for [`App::run_with_shutdown`].
///
/// If the signal handlers cannot be installed, it never resolves.
///
/// ```no_run
/// # use zapline::prelude::*;
/// # async fn demo(listener: tokio::net::TcpListener) -> std::io::Result<()> {
/// let (tx, rx) = tokio::sync::oneshot::channel::<()>();
/// let stop = async move {
///     tokio::select! {
///         _ = zapline::server::shutdown_signal() => {}
///         _ = rx => {}
///     }
/// };
/// App::new().serve_with_shutdown(listener, stop).await
/// # }
/// ```
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut int), Ok(mut term)) => {
                tokio::select! {
                    _ = int.recv() => {}
                    _ = term.recv() => {}
                }
            }
            _ => std::future::pending().await,
        }
    }
    #[cfg(windows)]
    {
        // Ctrl-C/Ctrl-Break from a console, CTRL_CLOSE when the console window
        // closes, and CTRL_SHUTDOWN from `docker stop` / system shutdown.
        // Windows force-terminates soon after close/shutdown events (about
        // 5s and 20s), so keep `shutdown_timeout` short for those setups.
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
        match (ctrl_c(), ctrl_break(), ctrl_close(), ctrl_shutdown()) {
            (Ok(mut c), Ok(mut b), Ok(mut close), Ok(mut shut)) => {
                tokio::select! {
                    _ = c.recv() => {}
                    _ = b.recv() => {}
                    _ = close.recv() => {}
                    _ = shut.recv() => {}
                }
            }
            _ => {
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

// ---- App builder methods -------------------------------------------------------

impl App {
    /// Choose how workers schedule connections (see [`RuntimeMode`]).
    pub fn runtime(mut self, mode: RuntimeMode) -> Self {
        self.config.runtime = mode;
        self
    }

    /// Serve HTTP/2 (h2c with prior knowledge, and ALPN `h2` over TLS) next
    /// to HTTP/1.1. On by default with the `http2` feature; enabling it
    /// without that feature makes `run`/`serve` fail with `InvalidInput`.
    pub fn http2(mut self, enabled: bool) -> Self {
        self.config.http2 = enabled;
        self
    }

    /// How long shutdown waits for in-flight requests before closing the
    /// remaining connections (default 30s).
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.config.shutdown_timeout = timeout;
        self
    }

    /// Cap open connections per worker (in total for
    /// [`RuntimeMode::MultiThread`] and [`App::serve`]). At the limit the
    /// server stops accepting until a connection closes. Upgraded WebSocket
    /// connections keep their slot (see [`ServerConfig::max_upgraded`]).
    ///
    /// ```
    /// use zapline::prelude::*;
    ///
    /// let mut app = App::new().max_connections(10_000);
    /// assert_eq!(app.server_config_mut().max_connections, Some(10_000));
    /// ```
    pub fn max_connections(mut self, n: usize) -> Self {
        self.config.max_connections = Some(n.max(1));
        self
    }

    /// Like [`App::run`], but shuts down gracefully when `signal` resolves
    /// instead of on SIGINT/SIGTERM.
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use zapline::prelude::*;
    ///
    /// fn main() -> std::io::Result<()> {
    ///     let stop = tokio::time::sleep(Duration::from_secs(60)); // or a channel, a signal...
    ///     App::new()
    ///         .get("/", || async { "hi" })
    ///         .run_with_shutdown("0.0.0.0:8080", stop)
    /// }
    /// ```
    pub fn run_with_shutdown<F>(self, addr: impl ToSocketAddrs, signal: F) -> io::Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut app = self;
        stash(&mut app, |o| o.shutdown = Some(Box::pin(signal)));
        app.run(addr)
    }

    /// Like [`App::serve`], but shuts down gracefully when `signal` resolves
    /// instead of on SIGINT/SIGTERM. Returns once every connection is closed
    /// and the shutdown hooks have run.
    ///
    /// ```no_run
    /// # use zapline::prelude::*;
    /// # async fn demo() -> std::io::Result<()> {
    /// let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    /// let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    /// let server = tokio::spawn(
    ///     App::new()
    ///         .get("/", || async { "hi" })
    ///         .serve_with_shutdown(listener, async { let _ = stopped.await; }),
    /// );
    /// // ...
    /// let _ = stop.send(());
    /// server.await.unwrap()?;
    /// # Ok(()) }
    /// ```
    pub async fn serve_with_shutdown<F>(self, listener: TcpListener, signal: F) -> io::Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut app = self;
        stash(&mut app, |o| o.shutdown = Some(Box::pin(signal)));
        app.serve(listener).await
    }

    /// Serve on a Unix domain socket (e.g. behind nginx) until SIGINT or
    /// SIGTERM. A stale socket file from an earlier run is replaced; the file
    /// is removed on shutdown. [`ConnInfo::remote_addr`] is `None`.
    ///
    /// [`ConnInfo::remote_addr`]: crate::ConnInfo::remote_addr
    #[cfg(unix)]
    pub fn run_unix(self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut app = self;
        let path = path.as_ref().to_path_buf();
        stash(&mut app, |o| o.unix = Some((path, serve_unix)));
        app.run(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// [`App::run_unix`] with a custom shutdown signal.
    #[cfg(unix)]
    pub fn run_unix_with_shutdown<F>(self, path: impl AsRef<Path>, signal: F) -> io::Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut app = self;
        stash(&mut app, |o| o.shutdown = Some(Box::pin(signal)));
        app.run_unix(path)
    }
}

// ---- plumbing between the App builders and App::run / App::serve ------------------

/// Options set by the `App::*_with_shutdown` / `App::run_unix` builders,
/// carried in the app's extensions so `App::run` stays the one entry point.
#[derive(Clone, Default)]
struct RunOptions(Arc<Mutex<Options>>);

#[derive(Default)]
pub(crate) struct Options {
    /// `None`: stop on SIGINT/SIGTERM.
    pub(crate) shutdown: Option<BoxFuture<()>>,
    /// Set by `App::run_unix`: listen here instead of on the TCP address.
    #[cfg(unix)]
    unix: Option<(PathBuf, ServeUnix)>,
}

/// Serves a Unix listener. Only `App::run_unix` creates one, so apps that
/// never call it do not contain the Unix-socket connection code (see
/// `server::conn` on why that matters for the TCP hot path).
#[cfg(unix)]
type ServeUnix = fn(std::os::unix::net::UnixListener, Worker) -> BoxFuture<io::Result<()>>;

#[cfg(unix)]
fn serve_unix(
    listener: std::os::unix::net::UnixListener,
    worker: Worker,
) -> BoxFuture<io::Result<()>> {
    Box::pin(
        async move { conn::serve(tokio::net::UnixListener::from_std(listener)?, worker).await },
    )
}

fn stash(app: &mut App, f: impl FnOnce(&mut Options)) {
    let opts = app.extensions_mut().get_or_insert_default::<RunOptions>();
    f(&mut opts.0.lock().unwrap_or_else(PoisonError::into_inner));
}

/// Remove the run options stashed by the builders (defaults if none).
pub(crate) fn take_options(app: &mut App) -> Options {
    app.extensions_mut()
        .remove::<RunOptions>()
        .map(|o| std::mem::take(&mut *o.0.lock().unwrap_or_else(PoisonError::into_inner)))
        .unwrap_or_default()
}

/// Where `App::run` listens.
enum Bind {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf, ServeUnix),
}

// ---- App::run ----------------------------------------------------------------------

/// Start the app and serve on worker threads until `shutdown` (or a
/// signal), then run the shutdown hooks. Blocks the calling thread.
pub(crate) fn run(prepared: Prepared, addr: SocketAddr, options: Options) -> io::Result<()> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(io::Error::other(
            "App::run blocks and starts its own runtimes; inside an async runtime use App::serve",
        ));
    }
    // Startup and shutdown hooks run on a small "system" runtime that lives
    // as long as the server, so resources they create keep being driven.
    let system = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("zapline-system")
        .enable_all()
        .build()?;
    let started = system.block_on(prepared.start());
    let Running {
        app,
        mut lifecycle,
        resources,
        config,
    } = match started {
        Ok(running) => running,
        Err(e) => {
            system.shutdown_timeout(Duration::from_secs(1));
            let msg = format!("startup failed: {e}{}", e.help());
            // `main` prints the returned error with `Debug`, which escapes
            // the fix's line breaks; show the readable version first.
            if e.diagnostic_code().is_some() {
                eprintln!("{msg}");
            }
            return Err(io::Error::other(msg));
        }
    };
    // Before binding: a failing instance never takes connections.
    if let Err(e) = system.block_on(ready::self_check(&app)) {
        system.block_on(lifecycle.run_shutdown(&resources));
        system.shutdown_timeout(Duration::from_secs(1));
        return Err(io::Error::other(format!("startup failed: {e}")));
    }
    #[cfg(unix)]
    let bind = options
        .unix
        .map_or(Bind::Tcp(addr), |(path, serve)| Bind::Unix(path, serve));
    #[cfg(not(unix))]
    let bind = Bind::Tcp(addr);
    let handshake = crate::dev::proxy_handshake();
    let result = run_workers(
        app,
        bind,
        config,
        options.shutdown,
        system.handle(),
        handshake.as_deref(),
    );
    system.block_on(lifecycle.run_shutdown(&resources));
    // Don't hang on blocking tasks that ignore shutdown (plain drop would
    // wait for them forever).
    system.shutdown_timeout(Duration::from_secs(1));
    result
}

/// A bound, not yet registered listener.
enum StdListener {
    Tcp(std::net::TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener, ServeUnix),
}

impl StdListener {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            StdListener::Tcp(l) => l.local_addr(),
            #[cfg(unix)]
            StdListener::Unix(..) => Err(io::Error::other("unix socket has no IP address")),
        }
    }

    /// Register with the current runtime and serve until shutdown.
    async fn serve(self, worker: Worker) -> io::Result<()> {
        match self {
            StdListener::Tcp(l) => conn::serve(TcpListener::from_std(l)?, worker).await,
            #[cfg(unix)]
            StdListener::Unix(l, serve) => serve(l, worker).await,
        }
    }
}

fn run_workers(
    app: AppInner,
    bind: Bind,
    config: ServerConfig,
    shutdown: Option<BoxFuture<()>>,
    system: &tokio::runtime::Handle,
    handshake: Option<&Path>,
) -> io::Result<()> {
    // Under `zapline dev`: listen on a free loopback port behind its proxy,
    // which serves the address the app asked for (see `ENV_DEV_PROXY`).
    let (bind, proxied) = match (bind, handshake) {
        (Bind::Tcp(addr), Some(_)) if !config.is_tls() => {
            (Bind::Tcp(crate::dev::internal_addr(addr)), Some(addr))
        }
        (bind, _) => (bind, None),
    };
    let workers = config.worker_count();
    let per_core = config.runtime == RuntimeMode::PerCore
        && (REUSEPORT_BALANCES || !matches!(bind, Bind::Tcp(_)));
    let listeners = if per_core {
        bind_listeners(&bind, &config, workers, true)?
    } else {
        bind_listeners(&bind, &config, 1, ready::reuse_port_requested())?
    };
    // Identity of the socket file we created, so cleanup never unlinks a
    // replacement instance's socket bound at the same path while we drained.
    #[cfg(unix)]
    let socket_id = match &bind {
        Bind::Unix(path, _) => file_id(path),
        _ => None,
    };

    if let Some(path) = handshake {
        let (addr, internal) = match (&bind, proxied) {
            #[cfg(unix)]
            (Bind::Unix(path, _), _) => (format!("unix:{}", path.display()), None),
            (_, Some(asked)) => (asked.to_string(), Some(listeners[0].local_addr()?)),
            (_, None) => (listeners[0].local_addr()?.to_string(), None),
        };
        crate::dev::write_handshake(path, &addr, internal);
    }

    // For `zapline mcp`: the address this process really listens on (the
    // internal one under the dev proxy), reachable from this machine.
    if let Ok(addr) = listeners[0].local_addr() {
        crate::dev::announce(addr, config.is_tls());
    }
    if config.banner {
        let url = match (&bind, &listeners[0]) {
            #[cfg(unix)]
            (Bind::Unix(path, _), _) => format!("unix:{}", path.display()),
            (_, listener) => {
                let scheme = if config.is_tls() { "https" } else { "http" };
                match proxied {
                    Some(asked) => format!(
                        "{scheme}://{asked} (zapline dev proxy, app on {})",
                        listener.local_addr()?
                    ),
                    None => format!("{scheme}://{}", listener.local_addr()?),
                }
            }
        };
        let mode = if per_core {
            "thread-per-core"
        } else {
            "multi-threaded"
        };
        eprintln!(
            "zapline listening on {url} ({workers} workers, {mode}, {})",
            config.protocols()
        );
    }

    // Listening (the kernel queues connections until the workers accept).
    ready::notify("READY=1");

    let signals: Vec<Signal> = listeners
        .iter()
        .map(|_| Signal::new(config.drain_grace))
        .collect();
    let trigger = Trigger::new(signals.clone());
    let driver = system.spawn(drive(
        shutdown,
        trigger.clone(),
        config.banner.then_some(config.shutdown_timeout),
    ));

    let proto = Proto::new(&config);
    let result = if per_core {
        run_per_core(app, listeners, signals, &trigger, &proto, &config)
    } else {
        let worker = Worker::new(&app, &proto, &config, signals[0].clone());
        let listener = listeners.into_iter().next().expect("one listener");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("zapline-worker")
            .enable_all()
            .build()?;
        let result = rt.block_on(listener.serve(worker));
        rt.shutdown_timeout(Duration::from_secs(1));
        result
    };

    driver.abort();
    #[cfg(unix)]
    if let Bind::Unix(path, _) = &bind {
        if socket_id.is_some() && file_id(path) == socket_id {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

/// `(device, inode)` of the file at `path`, if any.
#[cfg(unix)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

fn run_per_core(
    app: AppInner,
    listeners: Vec<StdListener>,
    signals: Vec<Signal>,
    trigger: &Trigger,
    proto: &Proto,
    config: &ServerConfig,
) -> io::Result<()> {
    let mut handles = Vec::with_capacity(listeners.len());
    let mut result = Ok(());
    for (i, (listener, signal)) in listeners.into_iter().zip(signals).enumerate() {
        let worker = Worker::new(&app, proto, config, signal);
        let on_error = trigger.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("zapline-worker-{i}"))
            .spawn(move || -> io::Result<()> {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .and_then(|rt| {
                        let r = rt.block_on(listener.serve(worker));
                        rt.shutdown_timeout(Duration::from_secs(1));
                        r
                    });
                if result.is_err() {
                    // One worker failing takes the whole server down, gracefully.
                    on_error.drain();
                }
                result
            });
        match spawned {
            Ok(h) => handles.push(h),
            Err(e) => {
                trigger.drain();
                result = Err(e);
                break;
            }
        }
    }
    for h in handles {
        let r = h
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("worker thread panicked")));
        if result.is_ok() {
            result = r;
        }
    }
    result
}

/// Wait for the shutdown signal, then tell every worker to drain.
async fn drive(shutdown: Option<BoxFuture<()>>, trigger: Trigger, announce: Option<Duration>) {
    let announce = |msg: &str| {
        ready::notify("STOPPING=1");
        if let Some(timeout) = announce {
            eprintln!("zapline: {msg} (waiting up to {timeout:?} for open connections)");
        }
    };
    match shutdown {
        Some(signal) => {
            signal.await;
            announce("shutting down");
            trigger.drain();
        }
        None => {
            shutdown_signal().await;
            announce("shutting down, signal again to force");
            trigger.drain();
            shutdown_signal().await;
            trigger.force();
        }
    }
}

/// Bind `n` listeners. With `reuse_port`, TCP listeners share the port via
/// `SO_REUSEPORT` (which also lets a new server start while an old one
/// drains).
fn bind_listeners(
    bind: &Bind,
    config: &ServerConfig,
    n: usize,
    reuse_port: bool,
) -> io::Result<Vec<StdListener>> {
    match bind {
        Bind::Tcp(addr) => {
            let first = bind_tcp(*addr, config.backlog, reuse_port)?;
            // Reuse the resolved port so `:0` binds every worker to the same one.
            let local = first.local_addr()?;
            let mut listeners = vec![StdListener::Tcp(first)];
            for _ in 1..n {
                listeners.push(StdListener::Tcp(bind_tcp(local, config.backlog, true)?));
            }
            Ok(listeners)
        }
        #[cfg(unix)]
        Bind::Unix(path, serve) => {
            // Unix sockets have no SO_REUSEPORT balancing: workers share
            // one socket through duplicated descriptors.
            let first = bind_unix(path, config.backlog)?;
            let mut listeners = Vec::with_capacity(n);
            for _ in 1..n {
                listeners.push(StdListener::Unix(first.try_clone()?, *serve));
            }
            listeners.push(StdListener::Unix(first, *serve));
            Ok(listeners)
        }
    }
}

fn bind_tcp(addr: SocketAddr, backlog: i32, reuse_port: bool) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    // On Unix, SO_REUSEADDR only allows rebinding over TIME_WAIT connections.
    // On Windows it lets *other* processes bind the same port while we listen
    // (port hijacking), and Windows' default already allows quick restarts,
    // so leave it off there.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    #[cfg(all(
        unix,
        not(any(target_os = "solaris", target_os = "illumos", target_os = "cygwin"))
    ))]
    if reuse_port {
        socket.set_reuse_port(true)?;
    }
    #[cfg(not(all(
        unix,
        not(any(target_os = "solaris", target_os = "illumos", target_os = "cygwin"))
    )))]
    let _ = reuse_port;
    socket.set_tcp_nodelay(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(backlog)?;
    Ok(socket.into())
}

#[cfg(unix)]
fn bind_unix(path: &Path, backlog: i32) -> io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::FileTypeExt;

    // Replace a socket file left behind by a dead server, but never a live
    // server's socket or anything that is not a socket.
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another server is listening on {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::unix(path)?)?;
    socket.listen(backlog)?;
    Ok(socket.into())
}

// ---- App::serve --------------------------------------------------------------------

/// Start the app and serve `listener` on the current runtime until
/// `shutdown` (or a signal), then run the shutdown hooks.
pub(crate) async fn serve(
    prepared: Prepared,
    listener: TcpListener,
    shutdown: Option<BoxFuture<()>>,
) -> io::Result<()> {
    let Running {
        app,
        mut lifecycle,
        resources,
        config,
    } = prepared
        .start()
        .await
        .map_err(|e| io::Error::other(format!("startup failed: {e}{}", e.help())))?;

    if let Ok(addr) = listener.local_addr() {
        crate::dev::announce(addr, false);
    }
    let signal = Signal::new(config.drain_grace);
    let driver = tokio::spawn(drive(shutdown, Trigger::new(vec![signal.clone()]), None));
    let worker = Worker::new(&app, &Proto::new(&config), &config, signal);
    let result = conn::serve(listener, worker).await;
    driver.abort();

    lifecycle.run_shutdown(&resources).await;
    result
}
