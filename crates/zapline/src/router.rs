//! Route registration (`Routes`) and the compiled radix-tree router.

use std::sync::Arc;

use http::header::ALLOW;
use http::{HeaderValue, Method};

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

#[derive(Clone)]
enum MethodKey {
    Any,
    One(Method),
}

struct Entry {
    method: MethodKey,
    path: String,
    endpoint: BoxEndpoint,
}

/// A group of routes, optionally sharing middleware. Nest it under a prefix
/// with [`Routes::nest`] or [`App::nest`](crate::App::nest).
#[derive(Default)]
pub struct Routes {
    entries: Vec<Entry>,
    middleware: Vec<Arc<dyn Middleware>>,
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

    pub fn route<H: Handler<T>, T: 'static>(
        mut self,
        method: Method,
        path: &str,
        handler: H,
    ) -> Self {
        self.entries.push(Entry {
            method: MethodKey::One(method),
            path: normalize(path),
            endpoint: boxed(handler),
        });
        self
    }

    /// Match every method not otherwise registered on `path`.
    pub fn any<H: Handler<T>, T: 'static>(mut self, path: &str, handler: H) -> Self {
        self.entries.push(Entry {
            method: MethodKey::Any,
            path: normalize(path),
            endpoint: boxed(handler),
        });
        self
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

    /// Bake group middleware into each endpoint.
    fn finish(self) -> Vec<Entry> {
        if self.middleware.is_empty() {
            return self.entries;
        }
        let chain: Arc<[Arc<dyn Middleware>]> = self.middleware.into();
        self.entries
            .into_iter()
            .map(|e| Entry {
                endpoint: Arc::new(Wrapped {
                    chain: chain.clone(),
                    endpoint: e.endpoint,
                }),
                ..e
            })
            .collect()
    }

    pub(crate) fn build(self, fallback: Option<BoxEndpoint>) -> Router {
        let mut router = Router {
            matcher: matchit::Router::new(),
            slots: Vec::new(),
            fallback,
        };
        let mut index = std::collections::HashMap::<String, usize>::new();

        for e in self.finish() {
            let i = *index.entry(e.path.clone()).or_insert_with(|| {
                if let Err(err) = router.matcher.insert(e.path.clone(), router.slots.len()) {
                    panic!("invalid route `{}`: {err}", e.path);
                }
                router.slots.push(Slot::new(param_names(&e.path)));
                router.slots.len() - 1
            });
            router.slots[i].set(e.method, e.endpoint, &e.path);
        }
        for s in &mut router.slots {
            s.finalize();
        }
        router
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

/// Per-path handlers, with the common methods in fixed fields so dispatch is
/// a match instead of a search.
struct Slot {
    names: &'static [&'static str],
    get: Option<BoxEndpoint>,
    post: Option<BoxEndpoint>,
    put: Option<BoxEndpoint>,
    patch: Option<BoxEndpoint>,
    delete: Option<BoxEndpoint>,
    head: Option<BoxEndpoint>,
    options: Option<BoxEndpoint>,
    other: Vec<(Method, BoxEndpoint)>,
    any: Option<BoxEndpoint>,
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

    fn set(&mut self, method: MethodKey, ep: BoxEndpoint, path: &str) {
        let slot = match method {
            MethodKey::Any => &mut self.any,
            MethodKey::One(m) => match m {
                Method::GET => &mut self.get,
                Method::POST => &mut self.post,
                Method::PUT => &mut self.put,
                Method::PATCH => &mut self.patch,
                Method::DELETE => &mut self.delete,
                Method::HEAD => &mut self.head,
                Method::OPTIONS => &mut self.options,
                m => {
                    if self.other.iter().any(|(o, _)| *o == m) {
                        panic!("duplicate route: {m} {path}");
                    }
                    self.other.push((m, ep));
                    return;
                }
            },
        };
        if slot.is_some() {
            panic!("duplicate route for `{path}`");
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
    fn find(&self, m: &Method) -> Option<&BoxEndpoint> {
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
            Some(ep) => Routed::Pending(ep.call(req)),
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
