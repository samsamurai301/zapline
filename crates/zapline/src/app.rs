//! The application builder.

use std::any::TypeId;
use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Extensions, Method};

use crate::body::ReqBody;
use crate::error::ErrorReport;
use crate::handler::{boxed, BoxEndpoint, Handler};
use crate::lifecycle::{shutdown_hook, startup_hook, Lifecycle, Resources};
use crate::middleware::{Middleware, Next};
use crate::request::{ConnInfo, Params, Request, Shared, StateEntry};
use crate::response::Response;
use crate::router::{leak_entries, IntoRoutes, RouteInfo, Routed, Router, Routes};
use crate::server::ServerConfig;

/// Default maximum request body size: 2 MiB.
pub const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Env var: when set, `App::run` prints the route table as JSON and exits.
pub const ENV_PRINT_ROUTES: &str = "ZAPLINE_PRINT_ROUTES";

/// A reusable bundle of configuration: routes, middleware, state, hooks.
///
/// Anything `FnOnce(App) -> App` is a plugin, so small plugins can be plain
/// functions.
pub trait Plugin {
    fn install(self, app: App) -> App;
}

impl<F: FnOnce(App) -> App> Plugin for F {
    fn install(self, app: App) -> App {
        self(app)
    }
}

type BuildHook = Box<dyn FnOnce(&[&'static RouteInfo]) -> Routes + Send>;

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
    pub(crate) routes: Routes,
    pub(crate) middleware: Vec<Arc<dyn Middleware>>,
    pub(crate) states: Vec<StateEntry>,
    pub(crate) fallback: Option<BoxEndpoint>,
    pub(crate) body_limit: usize,
    pub(crate) config: ServerConfig,
    pub(crate) build_hooks: Vec<BuildHook>,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) extensions: Extensions,
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
            build_hooks: Vec::new(),
            lifecycle: Lifecycle::default(),
            extensions: Extensions::new(),
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

    /// Attach typed metadata to the most recently registered route.
    pub fn meta<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        self.routes = self.routes.meta(value);
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

    /// Direct access to the top-level route table, for plugins.
    pub fn routes_mut(&mut self) -> &mut Routes {
        &mut self.routes
    }

    /// Global middleware. Runs before routing, for every request (including
    /// 404s), in the order added. [`Request::route`] is not yet known here.
    pub fn wrap(mut self, mw: impl Middleware) -> Self {
        self.middleware.push(Arc::new(mw));
        self
    }

    /// Route middleware. Runs after routing for every matched route, so
    /// [`Request::route`] and [`Request::matched_path`] are available
    /// (ideal for metrics, auth policies and tracing).
    pub fn wrap_route(mut self, mw: impl Middleware) -> Self {
        self.routes = std::mem::take(&mut self.routes).wrap(mw);
        self
    }

    /// Register shared state, extracted in handlers with `State<T>`.
    /// Registering the same type twice replaces the earlier value.
    pub fn state<T: Clone + Send + Sync + 'static>(self, value: T) -> Self {
        self.state_arc(Arc::new(value))
    }

    /// Register shared state that is already behind an `Arc`, keyed by `T`.
    pub fn state_arc<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        let id = TypeId::of::<T>();
        self.states.retain(|(t, _)| *t != id);
        self.states.push((id, value));
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

    pub fn server_config_mut(&mut self) -> &mut ServerConfig {
        &mut self.config
    }

    /// Install a [`Plugin`].
    pub fn plugin(self, plugin: impl Plugin) -> Self {
        plugin.install(self)
    }

    /// Typed storage for plugins to keep builder-time configuration.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    /// Run `hook` once the route table is final. It sees every registered
    /// route (with metadata) and may return extra routes to add — this is how
    /// the OpenAPI plugin generates `/openapi.json`. Hooks run in order; each
    /// sees routes added by earlier hooks.
    pub fn on_build<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(&[&'static RouteInfo]) -> Routes + Send + 'static,
    {
        self.build_hooks.push(Box::new(hook));
        self
    }

    /// Run `hook` before the server starts accepting connections. Values
    /// inserted into [`Resources`] become available as `State<T>`.
    /// Startup fails (and `run` returns the error) if a hook fails.
    pub fn on_startup<F, Fut>(mut self, hook: F) -> Self
    where
        F: FnOnce(Resources) -> Fut + Send + 'static,
        Fut: Future<Output = crate::Result<()>> + Send + 'static,
    {
        self.lifecycle.startup.push(startup_hook(hook));
        self
    }

    /// Run `hook` after the server stops (in reverse registration order).
    pub fn on_shutdown<F, Fut>(mut self, hook: F) -> Self
    where
        F: FnOnce(Resources) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.lifecycle.shutdown.push(shutdown_hook(hook));
        self
    }

    /// Customize how every error response (extractor rejections, handler
    /// errors, 404/405) is rendered.
    pub fn error_handler<F>(self, f: F) -> Self
    where
        F: Fn(&ErrorReport) -> Response + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        self.wrap(move |req: Request, next: Next| {
            let f = f.clone();
            async move {
                let res = next.run(req).await;
                match res.extensions().get::<ErrorReport>() {
                    Some(report) => f(report),
                    None => res,
                }
            }
        })
    }

    /// Finalize routes and run build hooks. Synchronous: no I/O, no startup
    /// hooks — safe to call from tooling.
    pub(crate) fn prepare(mut self) -> Prepared {
        let mut infos = leak_entries(std::mem::take(&mut self.routes).finish());
        for hook in std::mem::take(&mut self.build_hooks) {
            let table: Vec<&'static RouteInfo> = infos.iter().map(|(i, _)| *i).collect();
            let extra = hook(&table);
            infos.extend(leak_entries(extra.finish()));
        }
        let table = infos.iter().map(|(i, _)| *i).collect();
        let router = Arc::new(Router::new(infos, self.fallback));
        Prepared {
            router,
            middleware: self.middleware.into(),
            table,
            resources: Resources::from_entries(self.states),
            body_limit: self.body_limit,
            lifecycle: self.lifecycle,
            config: self.config,
        }
    }

    /// Bind `addr` and serve on a pool of per-core runtimes until shutdown.
    /// This blocks the calling thread; no `#[tokio::main]` needed.
    ///
    /// Startup hooks run on a small "system" runtime that lives as long as
    /// the server, so resources they create (pools, background jobs) keep
    /// being driven.
    pub fn run(self, addr: impl ToSocketAddrs) -> std::io::Result<()> {
        let addr: SocketAddr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("no address to bind"))?;
        let prepared = self.prepare();
        prepared.maybe_print_routes();

        let system = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zapline-system")
            .enable_all()
            .build()?;
        let Running {
            app,
            mut lifecycle,
            resources,
            config,
        } = system
            .block_on(prepared.start())
            .map_err(|e| std::io::Error::other(format!("startup failed: {e}")))?;
        let result = crate::server::run(app, addr, config);
        system.block_on(lifecycle.run_shutdown(&resources));
        result
    }

    /// Serve on an existing listener from inside your own Tokio runtime.
    pub async fn serve(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let prepared = self.prepare();
        let Running {
            app,
            mut lifecycle,
            resources,
            config,
        } = prepared
            .start()
            .await
            .map_err(|e| std::io::Error::other(format!("startup failed: {e}")))?;
        let result = crate::server::serve(Arc::new(app), listener, Arc::new(config)).await;
        lifecycle.run_shutdown(&resources).await;
        result
    }

    /// An in-process client for tests; no sockets involved. Startup hooks run
    /// lazily on the first request.
    pub fn test(self) -> crate::test::TestClient {
        crate::test::TestClient::new(self.prepare())
    }

    /// The finished route table (runs build hooks). Mostly for tooling.
    pub fn route_table(self) -> Vec<&'static RouteInfo> {
        self.prepare().table
    }
}

/// An app whose routes are final but whose startup hooks have not run.
pub(crate) struct Prepared {
    router: Arc<Router>,
    middleware: Arc<[Arc<dyn Middleware>]>,
    pub(crate) table: Vec<&'static RouteInfo>,
    resources: Resources,
    body_limit: usize,
    lifecycle: Lifecycle,
    config: ServerConfig,
}

/// A started app: startup hooks have run, ready to serve.
pub(crate) struct Running {
    pub(crate) app: AppInner,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) resources: Resources,
    pub(crate) config: ServerConfig,
}

impl Prepared {
    pub(crate) async fn start(mut self) -> crate::Result<Running> {
        self.lifecycle.run_startup(&self.resources).await?;
        let shared = Arc::new(Shared {
            states: self.resources.snapshot().into(),
            body_limit: self.body_limit,
        });
        Ok(Running {
            app: AppInner {
                router: self.router,
                middleware: self.middleware,
                shared,
            },
            lifecycle: self.lifecycle,
            resources: self.resources,
            config: self.config,
        })
    }

    fn maybe_print_routes(&self) {
        if std::env::var_os(ENV_PRINT_ROUTES).is_none() {
            return;
        }
        println!("{}", routes_json(&self.table));
        std::process::exit(0);
    }
}

/// `[{"method":"GET","path":"/x"}, ...]` without requiring serde_json.
pub(crate) fn routes_json(table: &[&'static RouteInfo]) -> String {
    let items: Vec<String> = table
        .iter()
        .map(|r| {
            let m = r.method().map_or("ANY", |m| m.as_str());
            format!(
                r#"{{"method":"{}","path":"{}"}}"#,
                m,
                r.path().replace('\\', "\\\\").replace('"', "\\\"")
            )
        })
        .collect();
    format!("[{}]", items.join(","))
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
    pub(crate) fn handle(&self, req: http::Request<ReqBody>, conn: ConnInfo) -> ResponseFuture {
        let (head, body) = req.into_parts();
        let req = Request {
            head,
            body: Some(body),
            params: Params::default(),
            shared: self.shared.clone(),
            route: None,
            conn,
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
