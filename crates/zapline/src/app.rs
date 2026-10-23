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
use crate::router::{leak_entries, Entry, IntoRoutes, RouteInfo, Routed, Router, Routes};
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
    /// errors, 404/405) is rendered. Headers of the original error response
    /// (e.g. `WWW-Authenticate`, `Allow`, `Retry-After`) are kept unless the
    /// new response sets them.
    pub fn error_handler<F>(self, f: F) -> Self
    where
        F: Fn(&ErrorReport) -> Response + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        self.wrap(move |req: Request, next: Next| {
            let f = f.clone();
            async move {
                let res = next.run(req).await;
                let Some(report) = res.extensions().get::<ErrorReport>() else {
                    return res;
                };
                let mut new = f(report);
                // Keep protocol headers of the original error (WWW-Authenticate,
                // Allow, Retry-After, Set-Cookie, ...) unless the handler set them.
                let new_headers = new.headers_mut();
                for name in res.headers().keys() {
                    if name == http::header::CONTENT_TYPE
                        || name == http::header::CONTENT_LENGTH
                        || new_headers.contains_key(name)
                    {
                        continue;
                    }
                    for v in res.headers().get_all(name) {
                        new_headers.append(name.clone(), v.clone());
                    }
                }
                new
            }
        })
    }

    /// Finalize routes and run build hooks. Synchronous: no I/O, no startup
    /// hooks — safe to call from tooling.
    pub(crate) fn prepare(mut self) -> Prepared {
        // Dev errors wrap everything (installed here, so no per-request
        // check when they are off).
        crate::dev::install(&mut self);
        // Route middleware (`wrap_route`) also applies to routes that build
        // hooks add (docs, metrics, health), so it truly covers every route.
        let mut routes = std::mem::take(&mut self.routes);
        let route_chain = routes.take_chain();
        let with_route_chain = |mut entries: Vec<Entry>| {
            if let Some(chain) = &route_chain {
                entries.iter_mut().for_each(|e| e.push_group(chain.clone()));
            }
            entries
        };
        let mut infos = leak_entries(with_route_chain(routes.finish()));
        for hook in std::mem::take(&mut self.build_hooks) {
            let table: Vec<&'static RouteInfo> = infos.iter().map(|(i, _)| *i).collect();
            let extra = hook(&table);
            infos.extend(leak_entries(with_route_chain(extra.finish())));
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

    /// Bind `addr` and serve on a pool of per-core runtimes. This blocks the
    /// calling thread; no `#[tokio::main]` needed.
    ///
    /// Serves until SIGINT/SIGTERM (Ctrl-C on Windows), then stops accepting,
    /// drains in-flight requests (up to [`ServerConfig::shutdown_timeout`]),
    /// and runs the shutdown hooks. A second signal forces an immediate stop.
    /// Use [`App::run_with_shutdown`] for a custom trigger.
    ///
    /// Startup hooks run on a small "system" runtime that lives as long as
    /// the server, so resources they create (pools, background jobs) keep
    /// being driven.
    pub fn run(self, addr: impl ToSocketAddrs) -> std::io::Result<()> {
        let addr: SocketAddr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("no address to bind"))?;
        let mut app = self;
        let options = crate::server::take_options(&mut app);
        let checked = crate::server::check_config(&app.config);
        let prepared = app.prepare();
        prepared.maybe_print_routes();
        checked?;
        crate::server::run(prepared, addr, options)
    }

    /// Serve on an existing listener from inside your own Tokio runtime.
    ///
    /// Like [`run`](Self::run), stops gracefully on SIGINT/SIGTERM. Use
    /// [`App::serve_with_shutdown`] for a custom trigger, or pass
    /// `std::future::pending()` to it to opt out of signal handling.
    pub async fn serve(self, listener: tokio::net::TcpListener) -> std::io::Result<()> {
        let mut app = self;
        let options = crate::server::take_options(&mut app);
        crate::server::check_config(&app.config)?;
        crate::server::serve(app.prepare(), listener, options).await
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
            #[cfg(feature = "ws")]
            upgrades: None,
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
        // The OpenAPI plugin exits while the app is built when asked for its
        // spec; still running means there is none. Say so instead of serving.
        if std::env::var_os("ZAPLINE_EXPORT_OPENAPI").is_some() {
            eprintln!(
                "zapline: ZAPLINE_EXPORT_OPENAPI is set but the app has no OpenAPI document\n\
                 hint: add `.openapi(OpenApi::new(\"My API\", \"1.0.0\"))` to the App"
            );
            // EX_CONFIG: `zapline dev` tells this apart from a crash.
            std::process::exit(78);
        }
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
    /// The request body limit (see [`App::body_limit`]).
    #[cfg(feature = "openapi")]
    pub(crate) fn body_limit(&self) -> usize {
        self.shared.body_limit
    }

    /// A copy for one worker thread. Shares the router but gets its own
    /// `Arc` handles so per-request refcount traffic stays core-local.
    pub(crate) fn for_worker(
        &self,
        #[cfg(feature = "ws")] upgrades: Option<crate::server::Upgrades>,
    ) -> AppInner {
        AppInner {
            router: self.router.clone(),
            middleware: self.middleware.clone(),
            shared: Arc::new(Shared {
                states: self.shared.states.clone(),
                body_limit: self.shared.body_limit,
                #[cfg(feature = "ws")]
                upgrades,
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

impl ResponseFuture {
    /// Poll once right away, with a no-op waker, so that a handler that does
    /// not wait completes before hyper touches the connection again (HTTP/1).
    ///
    /// The request holds slices of hyper's read buffer, and hyper reads from
    /// the connection (watching for EOF) before it first polls the response
    /// future. A request still alive at that point leaves the buffer shared,
    /// so the read allocates a fresh 8 KiB buffer, once per request. Polling
    /// here runs the extractors, which consume the request, first.
    ///
    /// `Future::poll` only owes a wake-up to the most recent waker, and
    /// hyper polls a pending future with the connection's waker in the same
    /// pass (or once the previous response is written), so nothing is lost.
    #[inline]
    pub(crate) fn poll_now(self) -> Self {
        match self {
            ResponseFuture::Pending(mut fut) => {
                let mut cx = Context::from_waker(std::task::Waker::noop());
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(res) => ResponseFuture::Ready(Some(res)),
                    Poll::Pending => ResponseFuture::Pending(fut),
                }
            }
            ready => ready,
        }
    }
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
