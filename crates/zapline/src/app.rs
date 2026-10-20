//! The application builder.

use std::any::{Any, TypeId};
use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::Method;

use crate::body::ReqBody;
use crate::handler::{boxed, BoxEndpoint, Handler};
use crate::middleware::{Middleware, Next};
use crate::request::{Params, Request, Shared};
use crate::response::Response;
use crate::router::{IntoRoutes, Routed, Router, Routes};

/// Default maximum request body size: 2 MiB.
pub const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Server tuning knobs. The defaults are what the benchmarks use.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Worker threads, each with its own runtime and `SO_REUSEPORT` listener.
    /// Defaults to the number of available CPUs.
    pub workers: Option<usize>,
    /// Abort connections that do not send complete headers in time
    /// (slowloris protection). `None` disables it.
    pub header_read_timeout: Option<std::time::Duration>,
    /// TCP listen backlog.
    pub backlog: i32,
    /// Print the listening address on startup.
    pub banner: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            workers: None,
            header_read_timeout: Some(std::time::Duration::from_secs(30)),
            backlog: 4096,
            banner: true,
        }
    }
}

/// A zapline application.
///
/// ```no_run
/// use zapline::prelude::*;
///
/// async fn hello() -> &'static str { "Hello, world!" }
///
/// fn main() -> std::io::Result<()> {
///     App::new().get("/", hello).run("0.0.0.0:8080")
/// }
/// ```
pub struct App {
    routes: Routes,
    middleware: Vec<Arc<dyn Middleware>>,
    states: Vec<(TypeId, Box<dyn Any + Send + Sync>)>,
    fallback: Option<BoxEndpoint>,
    body_limit: usize,
    pub(crate) config: ServerConfig,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! method_fns {
    ($($fn:ident => $m:ident),*) => {$(
        #[doc = concat!("Register a `", stringify!($m), "` handler.")]
        pub fn $fn<H: Handler<T>, T: 'static>(mut self, path: &str, handler: H) -> Self {
            self.routes = self.routes.route(Method::$m, path, handler);
            self
        }
    )*};
}

impl App {
    pub fn new() -> Self {
        App {
            routes: Routes::new(),
            middleware: Vec::new(),
            states: Vec::new(),
            fallback: None,
            body_limit: DEFAULT_BODY_LIMIT,
            config: ServerConfig::default(),
        }
    }

    method_fns!(get => GET, post => POST, put => PUT, patch => PATCH,
                delete => DELETE, head => HEAD, options => OPTIONS);

    pub fn route<H: Handler<T>, T: 'static>(
        mut self,
        method: Method,
        path: &str,
        handler: H,
    ) -> Self {
        self.routes = self.routes.route(method, path, handler);
        self
    }

    pub fn any<H: Handler<T>, T: 'static>(mut self, path: &str, handler: H) -> Self {
        self.routes = self.routes.any(path, handler);
        self
    }

    /// Add macro-annotated handlers (`#[get("/")]`), `Routes`, or tuples of them.
    pub fn mount(mut self, routes: impl IntoRoutes) -> Self {
        self.routes = self.routes.mount(routes);
        self
    }

    /// Mount a route group under a prefix.
    pub fn nest(mut self, prefix: &str, routes: Routes) -> Self {
        self.routes = self.routes.nest(prefix, routes);
        self
    }

    /// Global middleware. Runs before routing, for every request (including
    /// 404s), in the order added.
    pub fn wrap(mut self, mw: impl Middleware) -> Self {
        self.middleware.push(Arc::new(mw));
        self
    }

    /// Register shared state, extracted in handlers with `State<T>`.
    /// Registering the same type twice replaces the earlier value.
    pub fn state<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        let id = TypeId::of::<T>();
        self.states.retain(|(t, _)| *t != id);
        self.states.push((id, Box::new(value)));
        self
    }

    /// Handler for requests that match no route (default: JSON 404).
    pub fn fallback<H: Handler<T>, T: 'static>(mut self, handler: H) -> Self {
        self.fallback = Some(boxed(handler));
        self
    }

    /// Maximum buffered request body size in bytes.
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = bytes;
        self
    }

    /// Number of worker threads (defaults to the CPU count).
    pub fn workers(mut self, n: usize) -> Self {
        self.config.workers = Some(n.max(1));
        self
    }

    pub fn server_config(mut self, config: ServerConfig) -> Self {
        self.config = config;
        self
    }

    pub(crate) fn build(self) -> (AppInner, ServerConfig) {
        let router = Arc::new(self.routes.build(self.fallback));
        let shared = Arc::new(Shared {
            states: self.states.into(),
            body_limit: self.body_limit,
        });
        let inner = AppInner {
            router,
            middleware: self.middleware.into(),
            shared,
        };
        (inner, self.config)
    }

    /// Bind `addr` and serve forever on a pool of per-core runtimes.
    /// This blocks the calling thread; no `#[tokio::main]` needed.
    pub fn run(self, addr: impl ToSocketAddrs) -> std::io::Result<()> {
        let addr: SocketAddr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("no address to bind"))?;
        let (inner, config) = self.build();
        crate::server::run(inner, addr, config)
    }

    /// Serve on an existing listener from inside your own Tokio runtime.
    pub async fn serve(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let (inner, config) = self.build();
        crate::server::serve(Arc::new(inner), listener, Arc::new(config)).await
    }

    /// An in-process client for tests; no sockets involved.
    pub fn test(self) -> crate::test::TestClient {
        crate::test::TestClient::new(Arc::new(self.build().0))
    }
}

/// A built application.
pub(crate) struct AppInner {
    router: Arc<Router>,
    middleware: Arc<[Arc<dyn Middleware>]>,
    shared: Arc<Shared>,
}

impl AppInner {
    /// A copy for one worker thread. Shares the router but gets its own
    /// `Arc` handles so per-request refcount traffic stays core-local.
    pub(crate) fn for_worker(&self) -> AppInner {
        AppInner {
            router: self.router.clone(),
            middleware: self.middleware.clone(),
            shared: Arc::new(Shared {
                states: self.shared.states.clone(),
                body_limit: self.shared.body_limit,
            }),
        }
    }

    #[inline]
    pub(crate) fn handle(&self, req: http::Request<ReqBody>) -> ResponseFuture {
        let (head, body) = req.into_parts();
        let req = Request {
            head,
            body: Some(body),
            params: Params::default(),
            shared: self.shared.clone(),
        };
        if self.middleware.is_empty() {
            match self.router.route(req) {
                Routed::Ready(r) => ResponseFuture::Ready(Some(r)),
                Routed::Pending(f) => ResponseFuture::Pending(f),
            }
        } else {
            let next = Next::new(self.middleware.clone(), self.router.clone());
            ResponseFuture::Pending(next.run(req))
        }
    }
}

pub(crate) enum ResponseFuture {
    Ready(Option<Response>),
    Pending(crate::handler::BoxFuture<Response>),
}

impl Future for ResponseFuture {
    type Output = Result<Response, std::convert::Infallible>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            ResponseFuture::Ready(r) => Poll::Ready(Ok(r.take().expect("polled after completion"))),
            ResponseFuture::Pending(f) => f.as_mut().poll(cx).map(Ok),
        }
    }
}
