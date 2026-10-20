//! Route registration (`Routes`) and the compiled radix-tree router.

use std::sync::Arc;

use http::header::ALLOW;
use http::{Extensions, HeaderValue, Method};

use crate::handler::{boxed, BoxEndpoint, BoxFuture, Endpoint, Handler};
use crate::middleware::{Middleware, Wrapped};
use crate::request::Request;
use crate::response::{IntoResponse, Response};
use crate::Error;

/// Something that can add routes to a [`Routes`] table — a `Routes` value,
/// a function annotated with `#[get]`/`#[post]`/..., or a tuple of them.
pub trait IntoRoutes {
    fn into_routes(self, routes: Routes) -> Routes;
}

/// A registered route: method, path pattern and typed metadata.
///
/// Plugins read the full table at build time (see
/// [`App::on_build`](crate::App::on_build)); middleware and handlers read the
/// matched route at request time via [`Request::route`].
#[derive(Debug)]
pub struct RouteInfo {
    method: Option<Method>,
    path: String,
    meta: Extensions,
}

impl RouteInfo {
    /// `None` for routes registered with `any`.
    pub fn method(&self) -> Option<&Method> {
        self.method.as_ref()
    }
    /// The path pattern, e.g. `/users/{id}`.
    pub fn path(&self) -> &str {
        &self.path
    }
    /// Typed metadata attached with [`Routes::meta`] / [`Routes::meta_all`].
    pub fn meta<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.meta.get::<T>()
    }
    pub fn meta_map(&self) -> &Extensions {
        &self.meta
    }
}

#[derive(Clone)]
enum MethodKey {
    Any,
    One(Method),
}

impl Entry {
    /// Wrap this entry in an outer group chain.
    pub(crate) fn push_group(&mut self, chain: Arc<[Arc<dyn Middleware>]>) {
        self.groups.push(chain);
    }
}

pub(crate) struct Entry {
    method: MethodKey,
    path: String,
    endpoint: BoxEndpoint,
    /// Group middleware chains, innermost group first. Applied when the
    /// router is compiled so [`EndpointLayers`] can stay innermost.
    groups: Vec<Arc<[Arc<dyn Middleware>]>>,
    meta: Extensions,
}

/// Route metadata: middleware that runs *innermost* for a route, after all
/// global, route and group middleware and right before the handler.
///
/// Plugins use it for enforcement that must see everything outer middleware
/// did (e.g. auth policies verifying that authentication actually ran).
/// Layers are keyed by type: adding a layer of a type that is already
/// present keeps the existing one. Group-level layers (set through
/// [`Routes::defaults_mut`]) are combined with route-level ones.
#[derive(Clone, Default)]
pub struct EndpointLayers {
    layers: Vec<(std::any::TypeId, Arc<dyn Middleware>)>,
}

impl EndpointLayers {
    /// Add `mw` unless a layer of the same type is already present.
    pub fn add<M: Middleware>(&mut self, mw: M) {
        let id = std::any::TypeId::of::<M>();
        if !self.layers.iter().any(|(t, _)| *t == id) {
            self.layers.push((id, Arc::new(mw)));
        }
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    fn union(mut self, other: &EndpointLayers) -> Self {
        for (id, mw) in &other.layers {
            if !self.layers.iter().any(|(t, _)| t == id) {
                self.layers.push((*id, mw.clone()));
            }
        }
        self
    }

    fn chain(&self) -> Arc<[Arc<dyn Middleware>]> {
        self.layers.iter().map(|(_, mw)| mw.clone()).collect()
    }
}

/// A group of routes, optionally sharing middleware and metadata. Nest it
/// under a prefix with [`Routes::nest`] or [`App::nest`](crate::App::nest).
#[derive(Default)]
pub struct Routes {
    entries: Vec<Entry>,
    middleware: Vec<Arc<dyn Middleware>>,
    defaults: Extensions,
}

macro_rules! method_fns {
    ($($fn:ident => $m:ident),*) => {$(
        #[doc = concat!("Register a `", stringify!($m), "` handler.")]
        pub fn $fn<H: Handler<T>, T: 'static>(self, path: &str, handler: H) -> Self {
            self.route(Method::$m, path, handler)
        }
    )*};
}

impl Routes {
    pub fn new() -> Self {
        Self::default()
    }

    method_fns!(get => GET, post => POST, put => PUT, patch => PATCH,
                delete => DELETE, head => HEAD, options => OPTIONS);

    pub fn route<H: Handler<T>, T: 'static>(self, method: Method, path: &str, handler: H) -> Self {
        self.route_with(method, path, handler, Extensions::new())
    }

    /// Register a route together with its metadata (used by the route macros).
    pub fn route_with<H: Handler<T>, T: 'static>(
        mut self,
        method: Method,
        path: &str,
        handler: H,
        meta: Extensions,
    ) -> Self {
        self.entries.push(Entry {
            method: MethodKey::One(method),
            path: normalize(path),
            endpoint: boxed(handler),
            groups: Vec::new(),
            meta,
        });
        self
    }

    /// Match every method not otherwise registered on `path`.
    pub fn any<H: Handler<T>, T: 'static>(mut self, path: &str, handler: H) -> Self {
        self.entries.push(Entry {
            method: MethodKey::Any,
            path: normalize(path),
            endpoint: boxed(handler),
            groups: Vec::new(),
            meta: Extensions::new(),
        });
        self
    }

    /// Register a pre-built endpoint (for plugins that construct endpoints
    /// dynamically).
    pub fn endpoint(
        mut self,
        method: Option<Method>,
        path: &str,
        endpoint: Arc<dyn Endpoint>,
    ) -> Self {
        self.entries.push(Entry {
            method: method.map_or(MethodKey::Any, MethodKey::One),
            path: normalize(path),
            endpoint,
            groups: Vec::new(),
            meta: Extensions::new(),
        });
        self
    }

    /// Attach typed metadata to the most recently registered route.
    ///
    /// # Panics
    /// If no route has been registered on this `Routes` yet.
    pub fn meta<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        self.last_meta_mut().insert(value);
        self
    }

    /// Mutable access to the metadata of the most recently registered route.
    ///
    /// # Panics
    /// If no route has been registered on this `Routes` yet.
    pub fn last_meta_mut(&mut self) -> &mut Extensions {
        &mut self
            .entries
            .last_mut()
            .expect("`meta` must be called after registering a route")
            .meta
    }

    /// Attach metadata to every route in this group (routes keep their own
    /// value if they set one explicitly).
    pub fn meta_all<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        self.defaults.insert(value);
        self
    }

    /// Group-default metadata, for plugins that need read-modify-write access.
    pub fn defaults_mut(&mut self) -> &mut Extensions {
        &mut self.defaults
    }

    /// Add macro-annotated handlers, other `Routes`, or tuples of them.
    pub fn mount(self, routes: impl IntoRoutes) -> Self {
        routes.into_routes(self)
    }

    /// Middleware applied to this group's routes only (runs after routing).
    pub fn wrap(mut self, mw: impl Middleware) -> Self {
        self.middleware.push(Arc::new(mw));
        self
    }

    /// Mount `routes` under `prefix`.
    pub fn nest(mut self, prefix: &str, routes: Routes) -> Self {
        let prefix = normalize(prefix);
        for e in routes.finish() {
            let path = join(&prefix, &e.path);
            self.entries.push(Entry { path, ..e });
        }
        self
    }

    /// Mount `routes` at the same level.
    pub fn merge(self, routes: Routes) -> Self {
        self.nest("/", routes)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove and return this group's middleware chain.
    pub(crate) fn take_chain(&mut self) -> Option<Arc<[Arc<dyn Middleware>]>> {
        let mw = std::mem::take(&mut self.middleware);
        (!mw.is_empty()).then(|| mw.into())
    }

    /// Record group middleware and merge default metadata into each entry.
    pub(crate) fn finish(self) -> Vec<Entry> {
        let chain: Option<Arc<[Arc<dyn Middleware>]>> =
            (!self.middleware.is_empty()).then(|| self.middleware.into());
        let defaults = self.defaults;
        let has_defaults = !defaults.is_empty();
        self.entries
            .into_iter()
            .map(|mut e| {
                if let Some(chain) = &chain {
                    e.groups.push(chain.clone());
                }
                if has_defaults {
                    let layers = match (
                        defaults.get::<EndpointLayers>(),
                        e.meta.get::<EndpointLayers>(),
                    ) {
                        (Some(group), Some(own)) => Some(own.clone().union(group)),
                        _ => None,
                    };
                    let mut meta = defaults.clone();
                    meta.extend(e.meta);
                    if let Some(layers) = layers {
                        meta.insert(layers);
                    }
                    e.meta = meta;
                }
                e
            })
            .collect()
    }
}

impl IntoRoutes for Routes {
    fn into_routes(self, routes: Routes) -> Routes {
        routes.merge(self)
    }
}

macro_rules! tuple_into_routes {
    ($($t:ident),*) => {
        impl<$($t: IntoRoutes),*> IntoRoutes for ($($t,)*) {
            #[allow(non_snake_case)]
            fn into_routes(self, routes: Routes) -> Routes {
                let ($($t,)*) = self;
                $(let routes = $t.into_routes(routes);)*
                routes
            }
        }
    };
}
tuple_into_routes!(A);
tuple_into_routes!(A, B);
tuple_into_routes!(A, B, C);
tuple_into_routes!(A, B, C, D);
tuple_into_routes!(A, B, C, D, E);
tuple_into_routes!(A, B, C, D, E, F);
tuple_into_routes!(A, B, C, D, E, F, G);
tuple_into_routes!(A, B, C, D, E, F, G, H);
tuple_into_routes!(A, B, C, D, E, F, G, H, I);
tuple_into_routes!(A, B, C, D, E, F, G, H, I, J);
tuple_into_routes!(A, B, C, D, E, F, G, H, I, J, K);
tuple_into_routes!(A, B, C, D, E, F, G, H, I, J, K, L);
tuple_into_routes!(A, B, C, D, E, F, G, H, I, J, K, L, M);
tuple_into_routes!(A, B, C, D, E, F, G, H, I, J, K, L, M, N);
tuple_into_routes!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O);
tuple_into_routes!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P);

fn normalize(path: &str) -> String {
    let p = path.trim();
    let mut s = if p.starts_with('/') {
        p.to_owned()
    } else {
        format!("/{p}")
    };
    if s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

fn join(prefix: &str, path: &str) -> String {
    match (prefix, path) {
        ("/", p) => p.to_owned(),
        (pre, "/") => pre.to_owned(),
        (pre, p) => format!("{pre}{p}"),
    }
}

/// Parameter names in pattern order: `/a/{id}/{*rest}` -> `["id", "rest"]`.
/// Leaked once at startup so per-request params can hold `&'static str`.
fn param_names(path: &str) -> &'static [&'static str] {
    let mut names = Vec::new();
    let mut chars = path.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '{' if chars.peek().map(|&(_, c)| c) == Some('{') => {
                chars.next();
            }
            '{' => {
                let rest = &path[i + 1..];
                if let Some(end) = rest.find('}') {
                    let name = rest[..end].trim_start_matches('*');
                    names.push(&*Box::leak(name.to_owned().into_boxed_str()));
                }
            }
            _ => {}
        }
    }
    Box::leak(names.into_boxed_slice())
}

/// Leak finished entries into `'static` route infos. Routes live for the
/// lifetime of the process, so requests can point at their route for free.
pub(crate) fn leak_entries(entries: Vec<Entry>) -> Vec<(&'static RouteInfo, BoxEndpoint)> {
    entries
        .into_iter()
        .map(|e| {
            let mut endpoint = e.endpoint;
            if let Some(layers) = e.meta.get::<EndpointLayers>().filter(|l| !l.is_empty()) {
                endpoint = Arc::new(Wrapped {
                    chain: layers.chain(),
                    endpoint,
                });
            }
            for chain in e.groups {
                endpoint = Arc::new(Wrapped { chain, endpoint });
            }
            let info: &'static RouteInfo = Box::leak(Box::new(RouteInfo {
                method: match e.method {
                    MethodKey::Any => None,
                    MethodKey::One(m) => Some(m),
                },
                path: e.path,
                meta: e.meta,
            }));
            (info, endpoint)
        })
        .collect()
}

#[derive(Clone)]
struct Ep {
    endpoint: BoxEndpoint,
    info: &'static RouteInfo,
}

/// Per-path handlers, with the common methods in fixed fields so dispatch is
/// a match instead of a search.
struct Slot {
    names: &'static [&'static str],
    get: Option<Ep>,
    post: Option<Ep>,
    put: Option<Ep>,
    patch: Option<Ep>,
    delete: Option<Ep>,
    head: Option<Ep>,
    options: Option<Ep>,
    other: Vec<(Method, Ep)>,
    any: Option<Ep>,
    allow: HeaderValue,
}

impl Slot {
    fn new(names: &'static [&'static str]) -> Self {
        Slot {
            names,
            get: None,
            post: None,
            put: None,
            patch: None,
            delete: None,
            head: None,
            options: None,
            other: Vec::new(),
            any: None,
            allow: HeaderValue::from_static(""),
        }
    }

    fn set(&mut self, ep: Ep) {
        let path = ep.info.path();
        let slot = match ep.info.method() {
            None => &mut self.any,
            Some(m) => match *m {
                Method::GET => &mut self.get,
                Method::POST => &mut self.post,
                Method::PUT => &mut self.put,
                Method::PATCH => &mut self.patch,
                Method::DELETE => &mut self.delete,
                Method::HEAD => &mut self.head,
                Method::OPTIONS => &mut self.options,
                ref m => {
                    if self.other.iter().any(|(o, _)| o == m) {
                        panic!("duplicate route: {m} {path}");
                    }
                    self.other.push((m.clone(), ep));
                    return;
                }
            },
        };
        if slot.is_some() {
            let m = ep.info.method().map_or("ANY", |m| m.as_str());
            panic!("duplicate route: {m} {path}");
        }
        *slot = Some(ep);
    }

    fn finalize(&mut self) {
        let mut allow: Vec<&str> = Vec::new();
        for (m, s) in [
            ("GET", &self.get),
            (
                "HEAD",
                if self.head.is_some() {
                    &self.head
                } else {
                    &self.get
                },
            ),
            ("POST", &self.post),
            ("PUT", &self.put),
            ("PATCH", &self.patch),
            ("DELETE", &self.delete),
            ("OPTIONS", &self.options),
        ] {
            if s.is_some() {
                allow.push(m);
            }
        }
        let other: Vec<String> = self.other.iter().map(|(m, _)| m.to_string()).collect();
        allow.extend(other.iter().map(String::as_str));
        self.allow = HeaderValue::from_str(&allow.join(", ")).unwrap();
    }

    #[inline]
    fn find(&self, m: &Method) -> Option<&Ep> {
        let ep = match *m {
            Method::GET => self.get.as_ref(),
            Method::POST => self.post.as_ref(),
            Method::PUT => self.put.as_ref(),
            Method::PATCH => self.patch.as_ref(),
            Method::DELETE => self.delete.as_ref(),
            // hyper suppresses the body for HEAD, so GET handlers serve it.
            Method::HEAD => self.head.as_ref().or(self.get.as_ref()),
            Method::OPTIONS => self.options.as_ref(),
            _ => self.other.iter().find(|(o, _)| o == m).map(|(_, e)| e),
        };
        ep.or(self.any.as_ref())
    }
}

pub(crate) struct Router {
    matcher: matchit::Router<usize>,
    slots: Vec<Slot>,
    fallback: Option<BoxEndpoint>,
}

/// Either an immediate response (404/405) or a handler's future.
pub(crate) enum Routed {
    Ready(Response),
    Pending(BoxFuture<Response>),
}

impl Router {
    pub(crate) fn new(
        routes: Vec<(&'static RouteInfo, BoxEndpoint)>,
        fallback: Option<BoxEndpoint>,
    ) -> Router {
        let mut router = Router {
            matcher: matchit::Router::new(),
            slots: Vec::new(),
            fallback,
        };
        let mut index = std::collections::HashMap::<&str, usize>::new();
        for (info, endpoint) in routes {
            let i = *index.entry(info.path()).or_insert_with(|| {
                if let Err(err) = router.matcher.insert(info.path(), router.slots.len()) {
                    panic!("invalid route `{}`: {err}", info.path());
                }
                router.slots.push(Slot::new(param_names(info.path())));
                router.slots.len() - 1
            });
            router.slots[i].set(Ep { endpoint, info });
        }
        for s in &mut router.slots {
            s.finalize();
        }
        router
    }

    #[inline]
    pub(crate) fn route(&self, mut req: Request) -> Routed {
        let path = req.head.uri.path();
        let (slot, ep) = match self.matcher.at(path) {
            Ok(m) => {
                let slot = &self.slots[*m.value];
                if !m.params.is_empty() {
                    let base = path.as_ptr() as usize;
                    for (name, (_, v)) in slot.names.iter().zip(m.params.iter()) {
                        let start = v.as_ptr() as usize - base;
                        req.params.push(name, start, start + v.len());
                    }
                }
                (slot, slot.find(&req.head.method))
            }
            Err(_) => {
                return match &self.fallback {
                    Some(f) => Routed::Pending(f.call(req)),
                    None => Routed::Ready(Error::not_found().into_response()),
                };
            }
        };
        match ep {
            Some(ep) => {
                req.route = Some(ep.info);
                Routed::Pending(ep.endpoint.call(req))
            }
            None => {
                let mut res =
                    Error::new(http::StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
                        .into_response();
                res.headers_mut().insert(ALLOW, slot.allow.clone());
                Routed::Ready(res)
            }
        }
    }
}

impl Endpoint for Router {
    #[inline]
    fn call(&self, req: Request) -> BoxFuture<Response> {
        match self.route(req) {
            Routed::Ready(r) => Box::pin(std::future::ready(r)),
            Routed::Pending(f) => f,
        }
    }
}
