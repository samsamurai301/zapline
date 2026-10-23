//! In-process test client: exercises the full pipeline (middleware, routing,
//! extractors) without opening sockets.
//!
//! ```
//! # use zapline::prelude::*;
//! # #[tokio::main(flavor = "current_thread")] async fn main() {
//! let client = App::new().get("/", || async { "hi" }).test();
//! let res = client.get("/").send().await;
//! assert_eq!(res.status(), 200);
//! assert_eq!(res.text(), "hi");
//! # }
//! ```
//!
//! - **Requests**: [`TestRequest`] sets headers, a query string
//!   ([`query`](TestRequest::query)) and bodies:
//!   [`json`](TestRequest::json), [`form`](TestRequest::form),
//!   [`multipart`](TestRequest::multipart) (built with [`MultipartForm`]) or
//!   raw [`body`](TestRequest::body).
//! - **Cookies**: [`App::test`](crate::App::test) is stateless;
//!   [`TestClient::with_cookies`] keeps cookies across requests like a
//!   browser (`Domain`, `Path`, `Secure`, `SameSite`, expiry, `HttpOnly`
//!   and cookie prefixes), for login and session flows.
//! - **Origins**: [`TestClient::base_url`] picks the site the app is served
//!   on; [`TestRequest::from_origin`] and [`TestRequest::fetch_from`] send
//!   a request as if another page started it, with the `Origin` and
//!   `Sec-Fetch-*` headers and `SameSite` rules a browser applies, to test
//!   CSRF defenses against real cross-site requests.
//! - **Assertions**: [`TestResponse::assert_status`] and
//!   [`assert_header`](TestResponse::assert_header) chain, and their panic
//!   messages include the response body.
//! - **Fuzzing** (`openapi` feature): `client.fuzz().await.assert_clean()`
//!   sends hundreds of requests generated from the app's OpenAPI schemas
//!   to every route and reports panics, 5xx, undeclared statuses and
//!   responses that break their schema (see `TestClient::fuzz`).
//! - **Snapshots**: [`TestClient::replay_snapshots`] replays traffic
//!   recorded in the [dev console](crate::dev) and fails with a diff when a
//!   response changed.
//!
//! ```
//! # use zapline::prelude::*;
//! # #[tokio::main(flavor = "current_thread")] async fn main() {
//! let app = App::new()
//!     .post("/login", || async {
//!         ([(header::SET_COOKIE, HeaderValue::from_static("user=ada; Path=/"))], "welcome")
//!     })
//!     .get("/me", |headers: HeaderMap| async move {
//!         match headers.get(header::COOKIE) {
//!             Some(c) if c == "user=ada" => Ok("ada"),
//!             _ => Err(Error::unauthorized()),
//!         }
//!     });
//!
//! let client = app.test().with_cookies();
//! client.post("/login").send().await.assert_status(200);
//! client.get("/me").send().await.assert_status(StatusCode::OK);
//! # }
//! ```

use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::header::{CONTENT_TYPE, COOKIE, HOST, SET_COOKIE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt;

use crate::app::{AppInner, Prepared};
use crate::body::ReqBody;
use crate::lifecycle::{Lifecycle, Resources};
use crate::request::ConnInfo;
use crate::router::RouteInfo;

#[cfg(feature = "openapi")]
mod fuzz;
#[cfg(feature = "json")]
pub use crate::snapshot::{Replay, ReplayReport, ENV_UPDATE_SNAPSHOTS};
#[cfg(feature = "openapi")]
pub use fuzz::{
    FailureKind, Fuzz, FuzzConfig, FuzzFailure, FuzzReport, FuzzRequest, ENV_FUZZ_CASES,
    ENV_FUZZ_SEED,
};

struct Started {
    app: AppInner,
    lifecycle: tokio::sync::Mutex<Lifecycle>,
    resources: Resources,
}

struct Inner {
    prepared: Mutex<Option<Prepared>>,
    started: tokio::sync::OnceCell<Started>,
    table: Vec<&'static RouteInfo>,
}

/// In-process client. Cheap to clone; clones share one app instance (and
/// one cookie store, see [`with_cookies`](Self::with_cookies)).
#[derive(Clone)]
pub struct TestClient {
    inner: Arc<Inner>,
    cookies: Option<Arc<Mutex<CookieStore>>>,
    /// Where relative request URIs go ([`base_url`](Self::base_url)).
    base: Option<Origin>,
}

impl TestClient {
    pub(crate) fn new(prepared: Prepared) -> Self {
        let table = prepared.table.clone();
        TestClient {
            inner: Arc::new(Inner {
                prepared: Mutex::new(Some(prepared)),
                started: tokio::sync::OnceCell::new(),
                table,
            }),
            cookies: None,
            base: None,
        }
    }

    /// This client with a fresh, empty cookie store, shared by its clones:
    /// like a browser, it stores every `Set-Cookie` it receives and sends
    /// matching cookies back in one `Cookie` header.
    ///
    /// The store follows RFC 6265 and the browser rules of RFC 6265bis:
    ///
    /// - **`Domain`**: without it a cookie is host-only; with it, it is sent
    ///   to that domain and its subdomains. A `Domain` the request's host
    ///   doesn't belong to, or a public suffix (`com`, `co.uk`), is
    ///   rejected.
    /// - **`Path`**: prefix matching, defaulting to the request's directory.
    /// - **`Secure`**: stored and sent only over secure requests: `https`,
    ///   or a loopback host such as the default `http://localhost` (as
    ///   browsers treat it). An insecure request can't overwrite a `Secure`
    ///   cookie.
    /// - **`SameSite`**: requests built with [`TestRequest::from_origin`] or
    ///   [`TestRequest::fetch_from`] are started by another page; when it is
    ///   cross-site, `Strict` cookies stay home and `Lax` ones (the default)
    ///   go only with safe top-level navigations. `SameSite=None` requires
    ///   `Secure`. Other requests are typed into the address bar: same-site.
    /// - **`Max-Age`/`Expires`**: `Max-Age` wins; a cookie set to expire is
    ///   deleted, and expired cookies are never sent.
    /// - **`HttpOnly`**: stored ([`cookies`](Self::cookies)) and hidden from
    ///   [`document_cookie`](Self::document_cookie).
    /// - **Prefixes**: `__Secure-` needs `Secure`; `__Host-` also needs
    ///   `Path=/` and no `Domain`.
    ///
    /// Sites are compared by registrable domain, using a small built-in
    /// list of public suffixes instead of the full Public Suffix List.
    /// Cookies set explicitly with [`TestRequest::header`] are sent first
    /// and take precedence over stored ones with the same name.
    ///
    /// Each call starts a new store, so two calls give two independent
    /// "browsers" on the same app.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let app = App::new().get("/", || async {
    ///     ([(header::SET_COOKIE, HeaderValue::from_static("seen=1"))], "hi")
    /// });
    /// let alice = app.test().with_cookies();
    /// alice.get("/").send().await;
    /// assert_eq!(alice.cookie("seen").as_deref(), Some("1"));
    /// # }
    /// ```
    pub fn with_cookies(mut self) -> Self {
        self.cookies = Some(Arc::default());
        self
    }

    /// The value of the stored cookie `name` (any path), if this client keeps
    /// cookies and has one that has not expired.
    pub fn cookie(&self, name: &str) -> Option<String> {
        let store = self.cookies.as_ref()?.lock().unwrap();
        let now = SystemTime::now();
        store
            .cookies
            .iter()
            .find(|c| c.cookie.name == name && !c.expired(now))
            .map(|c| c.cookie.value.clone())
    }

    /// Every unexpired stored cookie with its attributes, oldest first
    /// (empty without [`with_cookies`](Self::with_cookies)).
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// use zapline::test::CookieSameSite;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let app = App::new().get("/", || async {
    ///     ([(header::SET_COOKIE, HeaderValue::from_static("theme=dark; SameSite=Strict"))], "")
    /// });
    /// let browser = app.test().with_cookies();
    /// browser.get("/").send().await;
    /// assert_eq!(browser.cookies()[0].same_site, Some(CookieSameSite::Strict));
    /// # }
    /// ```
    pub fn cookies(&self) -> Vec<JarCookie> {
        let Some(store) = &self.cookies else {
            return Vec::new();
        };
        let now = SystemTime::now();
        store
            .lock()
            .unwrap()
            .cookies
            .iter()
            .filter(|c| !c.expired(now))
            .map(|c| c.cookie.clone())
            .collect()
    }

    /// What `document.cookie` returns to a script on the page at `url` (an
    /// absolute URL): the matching cookies that are not `HttpOnly`, as
    /// `a=1; b=2`. Use it to check what JavaScript can read, e.g. a CSRF
    /// token but never the session id.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let app = App::new().get("/", || async {
    ///     let mut res = "".into_response();
    ///     let h = res.headers_mut();
    ///     h.append(header::SET_COOKIE, HeaderValue::from_static("sid=s3cret; HttpOnly"));
    ///     h.append(header::SET_COOKIE, HeaderValue::from_static("csrf=t0ken"));
    ///     res
    /// });
    /// let browser = app.test().with_cookies();
    /// browser.get("/").send().await;
    /// assert_eq!(browser.document_cookie("http://localhost/"), "csrf=t0ken");
    /// # }
    /// ```
    ///
    /// # Panics
    /// If `url` is not an absolute `http(s)` URL.
    #[track_caller]
    pub fn document_cookie(&self, url: &str) -> String {
        let (origin, path) = page(url);
        let Some(store) = &self.cookies else {
            return String::new();
        };
        let cx = Context {
            target: &origin,
            path: &path,
            same_site: true,
            navigation: true,
            safe_method: true,
        };
        let mut store = store.lock().unwrap();
        let pairs: Vec<String> = store
            .matching(&cx, true)
            .into_iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        pairs.join("; ")
    }

    /// Store `set_cookie` (a `Set-Cookie` value) as if the page at `url` (an
    /// absolute URL) had sent it, with every rule of the store applied.
    /// Returns whether it was accepted. Use it to simulate cookies other
    /// sites set, such as a sibling subdomain planting one for the parent
    /// domain.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let browser = App::new().test().with_cookies();
    /// // evil.example.com can set a cookie for all of example.com...
    /// assert!(browser.set_cookie("https://evil.example.com/", "x=1; Domain=example.com"));
    /// // ...but not a `__Host-` one, nor one for another site.
    /// assert!(!browser.set_cookie(
    ///     "https://evil.example.com/",
    ///     "__Host-x=1; Domain=example.com; Path=/; Secure"
    /// ));
    /// assert!(!browser.set_cookie("https://evil.example.com/", "y=1; Domain=bank.com"));
    /// # }
    /// ```
    ///
    /// # Panics
    /// If the client keeps no cookies ([`with_cookies`](Self::with_cookies))
    /// or `url` is not an absolute `http(s)` URL.
    #[track_caller]
    pub fn set_cookie(&self, url: &str, set_cookie: &str) -> bool {
        let (origin, path) = page(url);
        let store = self
            .cookies
            .as_ref()
            .expect("set_cookie needs a cookie store: call `.with_cookies()` first");
        let cx = Context {
            target: &origin,
            path: &path,
            same_site: true,
            navigation: true,
            safe_method: true,
        };
        store.lock().unwrap().set(&cx, set_cookie)
    }

    /// Forget every stored cookie (like clearing the browser's cookies).
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let browser = App::new().test().with_cookies();
    /// browser.set_cookie("http://localhost/", "a=1");
    /// browser.clear_cookies();
    /// assert!(browser.cookies().is_empty());
    /// # }
    /// ```
    pub fn clear_cookies(&self) {
        if let Some(store) = &self.cookies {
            store.lock().unwrap().cookies.clear();
        }
    }

    /// Send relative request URIs (`/login`) to this origin instead of
    /// `http://localhost`: the cookie store keys cookies on its host and
    /// scheme, requests get a `Host` header, and `https` requests count as
    /// secure ([`ConnInfo::secure`](crate::ConnInfo::secure)).
    /// Absolute request URIs always use their own origin.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let app = App::new().get("/", |conn: ConnInfo, headers: HeaderMap| async move {
    ///     format!("{} {:?}", conn.secure, headers.get(header::HOST).unwrap())
    /// });
    /// let client = app.test().base_url("https://app.example.com");
    /// assert_eq!(client.get("/").send().await.text(), r#"true "app.example.com""#);
    /// # }
    /// ```
    ///
    /// # Panics
    /// If `url` is not an absolute `http(s)` URL.
    #[track_caller]
    pub fn base_url(mut self, url: &str) -> Self {
        self.base = Some(page(url).0);
        self
    }

    /// Run startup hooks now (they otherwise run on the first request).
    ///
    /// # Panics
    /// If a startup hook fails.
    pub async fn start(&self) -> &Self {
        self.started().await;
        self
    }

    async fn started(&self) -> &Started {
        self.inner
            .started
            .get_or_init(|| async {
                let prepared = self
                    .inner
                    .prepared
                    .lock()
                    .unwrap()
                    .take()
                    .expect("test app already started");
                match prepared.start().await {
                    Ok(r) => Started {
                        app: r.app,
                        lifecycle: tokio::sync::Mutex::new(r.lifecycle),
                        resources: r.resources,
                    },
                    Err(e) => panic!("startup failed: {e}"),
                }
            })
            .await
    }

    /// The started app's request body limit.
    #[cfg(feature = "openapi")]
    async fn body_limit(&self) -> usize {
        self.started().await.app.body_limit()
    }

    /// Application resources (state) after startup.
    pub async fn resources(&self) -> Resources {
        self.started().await.resources.clone()
    }

    /// Run shutdown hooks.
    pub async fn shutdown(&self) {
        if let Some(s) = self.inner.started.get() {
            s.lifecycle.lock().await.run_shutdown(&s.resources).await;
        }
    }

    /// The final route table (after build hooks).
    pub fn routes(&self) -> &[&'static RouteInfo] {
        &self.inner.table
    }

    pub fn request(&self, method: Method, uri: &str) -> TestRequest {
        TestRequest {
            client: self.clone(),
            builder: http::Request::builder().method(method).uri(uri),
            body: Bytes::new(),
            conn: ConnInfo {
                remote_addr: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
                local_addr: None,
                secure: false,
            },
            initiator: None,
        }
    }
    pub fn get(&self, uri: &str) -> TestRequest {
        self.request(Method::GET, uri)
    }
    pub fn post(&self, uri: &str) -> TestRequest {
        self.request(Method::POST, uri)
    }
    pub fn put(&self, uri: &str) -> TestRequest {
        self.request(Method::PUT, uri)
    }
    pub fn patch(&self, uri: &str) -> TestRequest {
        self.request(Method::PATCH, uri)
    }
    pub fn delete(&self, uri: &str) -> TestRequest {
        self.request(Method::DELETE, uri)
    }
}

#[must_use = "call .send().await to perform the request"]
pub struct TestRequest {
    client: TestClient,
    pub(crate) builder: http::request::Builder,
    pub(crate) body: Bytes,
    pub(crate) conn: ConnInfo,
    initiator: Option<Initiator>,
}

impl TestRequest {
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        self.builder = self.builder.header(key, value);
        self
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(self, token: &str) -> Self {
        self.header(http::header::AUTHORIZATION, format!("Bearer {token}"))
    }

    /// Send the request as a browser does when a page on `origin` starts a
    /// top-level navigation: following a link or submitting a form. The
    /// request gets the `Sec-Fetch-Site` and `Sec-Fetch-Mode: navigate`
    /// headers, plus `Origin` unless the method is `GET` or `HEAD`, and the
    /// cookie store applies `SameSite` when `origin` is cross-site (see
    /// [`TestClient::with_cookies`]). Headers set explicitly win.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// use zapline::middleware::SameOrigin;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let client = App::new()
    ///     .wrap(SameOrigin::new())
    ///     .post("/transfer", || async { "sent" })
    ///     .test()
    ///     .base_url("https://bank.example");
    /// // A form on another site posting to the bank is refused.
    /// let res = client.post("/transfer").from_origin("https://evil.example").send().await;
    /// assert_eq!(res.status(), 403);
    /// // The bank's own form works.
    /// let res = client.post("/transfer").from_origin("https://bank.example").send().await;
    /// assert_eq!(res.status(), 200);
    /// # }
    /// ```
    ///
    /// # Panics
    /// If `origin` is not an absolute `http(s)` URL.
    #[track_caller]
    pub fn from_origin(mut self, origin: &str) -> Self {
        self.initiator = Some(Initiator {
            origin: page(origin).0,
            navigation: true,
        });
        self
    }

    /// Send the request as a `fetch(.., { credentials: "include" })` from a
    /// script on `origin`: like [`from_origin`](Self::from_origin), but
    /// with `Sec-Fetch-Mode: cors`, an `Origin` header on every cross-origin
    /// request, and cross-site requests carry only `SameSite=None` cookies.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let client = App::new()
    ///     .get("/", |h: HeaderMap| async move { format!("{:?}", h.get("origin")) })
    ///     .test();
    /// let res = client.get("/").fetch_from("https://other.example").send().await;
    /// assert_eq!(res.text(), r#"Some("https://other.example")"#);
    /// # }
    /// ```
    ///
    /// # Panics
    /// If `origin` is not an absolute `http(s)` URL.
    #[track_caller]
    pub fn fetch_from(mut self, origin: &str) -> Self {
        self.initiator = Some(Initiator {
            origin: page(origin).0,
            navigation: false,
        });
        self
    }

    /// Pretend the request came from this peer address.
    pub fn remote_addr(mut self, addr: SocketAddr) -> Self {
        self.conn.remote_addr = Some(addr);
        self
    }

    /// Append `value`, URL-encoded, to the query string (after any query
    /// already in the URI).
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let client = App::new().get("/search", |uri: Uri| async move { uri.to_string() }).test();
    /// let res = client.get("/search?page=2").query(&[("q", "rust web")]).send().await;
    /// assert_eq!(res.text(), "/search?page=2&q=rust+web");
    /// # }
    /// ```
    ///
    /// # Panics
    /// If `value` cannot be URL-encoded (e.g. it contains nested structs).
    #[cfg(any(feature = "query", feature = "form"))]
    pub fn query<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Self {
        let query = serde_urlencoded::to_string(value).expect("failed to serialize test query");
        if query.is_empty() {
            return self;
        }
        if let Some(uri) = self.builder.uri_ref() {
            let mut uri = uri.to_string();
            match uri.find('?') {
                None => uri.push('?'),
                Some(i) if i + 1 < uri.len() && !uri.ends_with('&') => uri.push('&'),
                Some(_) => {}
            }
            uri.push_str(&query);
            self.builder = self.builder.uri(uri);
        }
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    #[cfg(feature = "json")]
    pub fn json<T: serde::Serialize>(self, value: &T) -> Self {
        let body = serde_json::to_vec(value).expect("failed to serialize test JSON body");
        self.header(CONTENT_TYPE, "application/json").body(body)
    }

    #[cfg(feature = "form")]
    pub fn form<T: serde::Serialize>(self, value: &T) -> Self {
        let body = serde_urlencoded::to_string(value).expect("failed to serialize test form");
        self.header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
    }

    /// A `multipart/form-data` body, as a browser submits a form with file
    /// inputs.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// use zapline::test::MultipartForm;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// # let client = App::new().post("/upload", |b: Bytes| async move { b }).test();
    /// let form = MultipartForm::new()
    ///     .text("title", "Holiday")
    ///     .file("photo", "beach.png", "image/png", vec![0x89, b'P', b'N', b'G']);
    /// let res = client.post("/upload").multipart(form).send().await;
    /// # assert_eq!(res.status(), 200);
    /// # }
    /// ```
    pub fn multipart(self, form: MultipartForm) -> Self {
        let (content_type, body) = form.encode();
        self.header(CONTENT_TYPE, content_type).body(body)
    }

    pub async fn send(self) -> TestResponse {
        self.send_with(true).await
    }

    /// Like `send`, but a `text/event-stream` response comes back with an
    /// empty body instead of being read until the stream ends (never, for
    /// most of them).
    #[cfg(feature = "openapi")]
    pub(crate) async fn send_skipping_streams(self) -> TestResponse {
        self.send_with(false).await
    }

    async fn send_with(self, read_streams: bool) -> TestResponse {
        let mut req = self
            .builder
            .body(ReqBody::Full(self.body))
            .expect("invalid test request");
        let base = self.client.base.as_ref();
        let target = match Origin::from_uri(req.uri()) {
            Some(origin) => origin,
            None => {
                if let Some(base) = base {
                    if !req.headers().contains_key(HOST) {
                        if let Ok(v) = HeaderValue::try_from(base.authority()) {
                            req.headers_mut().insert(HOST, v);
                        }
                    }
                }
                let https = base.is_some_and(|b| b.https);
                req.headers()
                    .get(HOST)
                    .and_then(|h| Origin::from_host(h, https))
                    .or_else(|| base.cloned())
                    .unwrap_or_else(Origin::localhost)
            }
        };
        let mut conn = self.conn;
        conn.secure |= target.https;
        let safe_method = matches!(
            *req.method(),
            Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
        );
        let (same_site, navigation) = match &self.initiator {
            Some(init) => {
                fetch_metadata(init, &target, req.method().clone(), req.headers_mut());
                (init.origin.same_site(&target), init.navigation)
            }
            None => (true, true),
        };
        let path = req.uri().path().to_owned();
        let cx = Context {
            target: &target,
            path: &path,
            same_site,
            navigation,
            safe_method,
        };
        if let Some(jar) = &self.client.cookies {
            jar.lock()
                .unwrap()
                .add_cookie_header(&cx, req.headers_mut());
        }
        let started = self.client.started().await;
        let res = started
            .app
            .handle(req, conn)
            .await
            .unwrap_or_else(|e| match e {});
        let (parts, body) = res.into_parts();
        if let Some(jar) = &self.client.cookies {
            jar.lock().unwrap().store(&cx, &parts.headers);
        }
        let streaming = parts
            .headers
            .get(CONTENT_TYPE)
            .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
        if streaming && !read_streams {
            return TestResponse {
                status: parts.status,
                headers: parts.headers,
                body: Bytes::new(),
            };
        }
        let body = body
            .collect()
            .await
            .map(|c| c.to_bytes())
            .expect("failed to read response body");
        TestResponse {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }
}

#[derive(Debug)]
pub struct TestResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl TestResponse {
    pub fn status(&self) -> StatusCode {
        self.status
    }
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
    /// The first value of header `name` (`None` if missing or not UTF-8).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    /// Every value of header `name`, in order (values that are not UTF-8 are
    /// skipped).
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// # let client = App::new().get("/", || async {
    /// #     let mut res = "".into_response();
    /// #     res.headers_mut().append(header::VARY, HeaderValue::from_static("origin"));
    /// #     res.headers_mut().append(header::VARY, HeaderValue::from_static("accept"));
    /// #     res
    /// # }).test();
    /// let res = client.get("/").send().await;
    /// assert_eq!(res.header_all("vary"), ["origin", "accept"]);
    /// # }
    /// ```
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect()
    }
    /// The `Set-Cookie` header for cookie `name` (the last one, if several).
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// # let client = App::new().get("/", || async {
    /// #     ([(header::SET_COOKIE, HeaderValue::from_static("sid=abc; Path=/; HttpOnly"))], "")
    /// # }).test();
    /// let res = client.get("/").send().await;
    /// let sid = res.cookie("sid").expect("no session cookie");
    /// assert_eq!(sid.value(), "abc");
    /// assert!(sid.http_only());
    /// # }
    /// ```
    pub fn cookie(&self, name: &str) -> Option<SetCookie<'_>> {
        self.headers
            .get_all(SET_COOKIE)
            .iter()
            .rev()
            .filter_map(|v| SetCookie::parse(v.to_str().ok()?))
            .find(|c| c.name() == name)
    }
    pub fn bytes(&self) -> &Bytes {
        &self.body
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    #[cfg(feature = "json")]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("response is not valid JSON ({e}): {}", self.text()))
    }

    /// Panic unless the status is `expected` (a [`StatusCode`] or a `u16`).
    /// The message includes the response body, which usually says why.
    ///
    /// ```
    /// # use zapline::prelude::*;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// # let client = App::new().post("/items", || async { (StatusCode::CREATED, "made") }).test();
    /// client
    ///     .post("/items")
    ///     .send()
    ///     .await
    ///     .assert_status(StatusCode::CREATED)
    ///     .assert_header("content-type", "text/plain; charset=utf-8");
    /// # }
    /// ```
    #[track_caller]
    pub fn assert_status<S>(&self, expected: S) -> &Self
    where
        StatusCode: PartialEq<S>,
        S: fmt::Debug,
    {
        if self.status != expected {
            panic!(
                "expected status {expected:?}, got {}\nresponse body: {}",
                self.status,
                self.body_snippet()
            );
        }
        self
    }

    /// Panic unless header `name` has the value `expected` (any of its values,
    /// if repeated).
    #[track_caller]
    pub fn assert_header(&self, name: &str, expected: &str) -> &Self {
        let values = self.header_all(name);
        if !values.contains(&expected) {
            panic!(
                "expected header `{name}: {expected}`, got {values:?} (status {})\nresponse body: {}",
                self.status,
                self.body_snippet()
            );
        }
        self
    }

    /// Panic unless the body is exactly `expected`.
    #[track_caller]
    pub fn assert_text(&self, expected: &str) -> &Self {
        if self.body != expected.as_bytes() {
            panic!(
                "expected body {expected:?} (status {})\nresponse body: {}",
                self.status,
                self.body_snippet()
            );
        }
        self
    }

    /// Panic unless the body contains `needle`.
    #[track_caller]
    pub fn assert_contains(&self, needle: &str) -> &Self {
        if !String::from_utf8_lossy(&self.body).contains(needle) {
            panic!(
                "expected the body to contain {needle:?} (status {})\nresponse body: {}",
                self.status,
                self.body_snippet()
            );
        }
        self
    }

    /// The body for panic messages, cut to a readable length.
    fn body_snippet(&self) -> String {
        const MAX: usize = 2048;
        if self.body.is_empty() {
            return "<empty>".into();
        }
        let text = String::from_utf8_lossy(&self.body[..self.body.len().min(MAX)]).into_owned();
        if self.body.len() > MAX {
            format!("{text}... ({} bytes total)", self.body.len())
        } else {
            text
        }
    }
}

// ---------------------------------------------------------------------------
// Cookies

/// A `Set-Cookie` response header, as returned by [`TestResponse::cookie`].
///
/// Attribute names are matched case-insensitively; when an attribute repeats,
/// the last one wins (as in browsers).
#[derive(Clone, Copy)]
pub struct SetCookie<'a> {
    name: &'a str,
    value: &'a str,
    attrs: &'a str,
}

impl<'a> SetCookie<'a> {
    /// Parse a `Set-Cookie` value; `None` without a `name=value` pair.
    pub fn parse(header: &'a str) -> Option<Self> {
        let (pair, attrs) = header.split_once(';').unwrap_or((header, ""));
        let (name, value) = pair.split_once('=')?;
        let name = name.trim();
        (!name.is_empty()).then(|| SetCookie {
            name,
            value: value.trim(),
            attrs,
        })
    }

    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The value exactly as sent (still percent-encoded, if it was).
    pub fn value(&self) -> &'a str {
        self.value
    }

    /// The value of attribute `name` (`Some("")` for flags like `HttpOnly`).
    pub fn attr(&self, name: &str) -> Option<&'a str> {
        self.attrs.rsplit(';').find_map(|a| {
            let (k, v) = a.split_once('=').unwrap_or((a, ""));
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    pub fn path(&self) -> Option<&'a str> {
        self.attr("Path")
    }

    pub fn domain(&self) -> Option<&'a str> {
        self.attr("Domain")
    }

    pub fn same_site(&self) -> Option<&'a str> {
        self.attr("SameSite")
    }

    pub fn http_only(&self) -> bool {
        self.attr("HttpOnly").is_some()
    }

    pub fn secure(&self) -> bool {
        self.attr("Secure").is_some()
    }

    /// `Max-Age` in seconds (`None` if absent or malformed).
    pub fn max_age(&self) -> Option<i64> {
        self.attr("Max-Age")?.parse().ok()
    }

    /// `Expires` (`None` if absent or not an HTTP date).
    pub fn expires(&self) -> Option<SystemTime> {
        httpdate::parse_http_date(self.attr("Expires")?).ok()
    }

    /// Whether this header deletes the cookie: `Max-Age` of zero or less, or
    /// (without `Max-Age`) an `Expires` in the past.
    pub fn is_removal(&self) -> bool {
        matches!(self.expiry(SystemTime::now()), Expiry::Expired)
    }

    /// `Max-Age` wins over `Expires` (RFC 6265, section 5.3).
    fn expiry(&self, now: SystemTime) -> Expiry {
        match (self.max_age(), self.expires()) {
            (Some(secs), _) if secs <= 0 => Expiry::Expired,
            (Some(secs), _) => now
                .checked_add(Duration::from_secs(secs.unsigned_abs()))
                .map_or(Expiry::Session, Expiry::At),
            (None, Some(at)) if at <= now => Expiry::Expired,
            (None, Some(at)) => Expiry::At(at),
            (None, None) => Expiry::Session,
        }
    }
}

impl fmt::Debug for SetCookie<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SetCookie({}={}", self.name, self.value)?;
        if !self.attrs.is_empty() {
            write!(f, ";{}", self.attrs)?;
        }
        f.write_str(")")
    }
}

enum Expiry {
    Expired,
    At(SystemTime),
    Session,
}

/// The fetch metadata headers a browser adds to a request `init` starts
/// (explicit ones win).
fn fetch_metadata(init: &Initiator, target: &Origin, method: Method, headers: &mut HeaderMap) {
    let site = if init.origin.same_origin(target) {
        "same-origin"
    } else if init.origin.same_site(target) {
        "same-site"
    } else {
        "cross-site"
    };
    let mode = if init.navigation { "navigate" } else { "cors" };
    let sends_origin = !matches!(method, Method::GET | Method::HEAD)
        || (!init.navigation && site != "same-origin");
    let mut add = |name: &'static str, value: String| {
        if let Ok(v) = HeaderValue::try_from(value) {
            headers.entry(HeaderName::from_static(name)).or_insert(v);
        }
    };
    add("sec-fetch-site", site.to_owned());
    add("sec-fetch-mode", mode.to_owned());
    if sends_origin {
        add("origin", init.origin.serialize());
    }
}

/// A cookie held by a [`TestClient::with_cookies`] store, as
/// [`TestClient::cookies`] lists it.
///
/// ```
/// # use zapline::prelude::*;
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let app = App::new().get("/", || async {
///     ([(header::SET_COOKIE, HeaderValue::from_static("sid=abc; Path=/; Secure; HttpOnly"))], "")
/// });
/// let browser = app.test().with_cookies();
/// browser.get("/").send().await;
/// let sid = &browser.cookies()[0];
/// assert_eq!((sid.name.as_str(), sid.domain.as_str()), ("sid", "localhost"));
/// assert!(sid.host_only && sid.secure && sid.http_only);
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct JarCookie {
    pub name: String,
    /// The value exactly as sent (still percent-encoded, if it was).
    pub value: String,
    /// The host that set it (when [`host_only`](Self::host_only)) or its
    /// `Domain` attribute, lowercased and without a leading dot.
    pub domain: String,
    /// Sent only to [`domain`](Self::domain) itself, not its subdomains
    /// (the cookie had no `Domain` attribute).
    pub host_only: bool,
    pub path: String,
    /// Sent only over secure requests (`https`, or a loopback host).
    pub secure: bool,
    /// Hidden from scripts ([`TestClient::document_cookie`]).
    pub http_only: bool,
    /// The `SameSite` attribute; `None` when absent, which browsers treat
    /// as `Lax`.
    pub same_site: Option<CookieSameSite>,
    /// When it expires; `None` for a session cookie.
    pub expires: Option<SystemTime>,
}

/// A cookie's `SameSite` attribute, see [`JarCookie::same_site`].
///
/// ```
/// use zapline::test::CookieSameSite;
/// assert_ne!(CookieSameSite::Lax, CookieSameSite::Strict);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CookieSameSite {
    /// Never sent on cross-site requests.
    Strict,
    /// Sent on cross-site requests only for top-level navigations with a
    /// safe method (following a link, a `GET` form).
    Lax,
    /// Always sent (browsers require `Secure` with it).
    None,
}

/// Who starts a request ([`TestRequest::from_origin`],
/// [`TestRequest::fetch_from`]).
#[derive(Clone, Debug)]
struct Initiator {
    origin: Origin,
    navigation: bool,
}

/// Scheme, host and port of a URL: what cookies and sites are keyed on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    https: bool,
    /// Lowercase; IPv6 addresses without brackets.
    host: String,
    port: Option<u16>,
}

impl Origin {
    fn localhost() -> Self {
        Origin {
            https: false,
            host: "localhost".into(),
            port: None,
        }
    }

    fn from_uri(uri: &http::Uri) -> Option<Self> {
        let https = match uri.scheme_str()?.to_ascii_lowercase().as_str() {
            "https" | "wss" => true,
            "http" | "ws" => false,
            _ => return None,
        };
        let host = uri.host()?;
        Some(Origin {
            https,
            host: unbracket(host).to_ascii_lowercase(),
            port: uri.port_u16(),
        })
    }

    /// The origin named by a `Host` header, with the base URL's scheme.
    fn from_host(host: &HeaderValue, https: bool) -> Option<Self> {
        let authority: http::uri::Authority = host.to_str().ok()?.parse().ok()?;
        Some(Origin {
            https,
            host: unbracket(authority.host()).to_ascii_lowercase(),
            port: authority.port_u16(),
        })
    }

    fn port(&self) -> u16 {
        self.port.unwrap_or(if self.https { 443 } else { 80 })
    }

    fn same_origin(&self, other: &Origin) -> bool {
        self.https == other.https && self.host == other.host && self.port() == other.port()
    }

    /// Schemeful same-site, as current browsers define it for cookies.
    fn same_site(&self, other: &Origin) -> bool {
        self.https == other.https
            && registrable_domain(&self.host) == registrable_domain(&other.host)
    }

    /// A secure context: `https`, or a loopback host (browsers treat
    /// `http://localhost` as secure, so `Secure` cookies work there).
    fn is_secure(&self) -> bool {
        self.https
            || self.host == "localhost"
            || self.host.ends_with(".localhost")
            || self
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }

    /// The `host[:port]` authority (`[..]` around IPv6).
    fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(p) => format!("{host}:{p}"),
            None => host,
        }
    }

    /// As browsers send it in the `Origin` header.
    fn serialize(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}", self.authority())
    }
}

fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

fn is_ip(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Public suffixes with more than one label that the store knows about.
/// Single labels (`com`, `localhost`) are always public suffixes. This is a
/// small stand-in for the Public Suffix List browsers ship.
const MULTI_LABEL_SUFFIXES: &[&str] = &[
    "co.uk",
    "org.uk",
    "ac.uk",
    "gov.uk",
    "com.au",
    "net.au",
    "org.au",
    "co.jp",
    "co.nz",
    "com.br",
    "co.in",
    "github.io",
    "herokuapp.com",
    "vercel.app",
    "netlify.app",
    "pages.dev",
];

fn is_public_suffix(domain: &str) -> bool {
    !domain.contains('.') || MULTI_LABEL_SUFFIXES.contains(&domain)
}

/// The registrable domain (the "site") of `host`: its public suffix plus
/// one label. IP addresses and single-label hosts are their own site.
fn registrable_domain(host: &str) -> &str {
    if is_ip(host) {
        return host;
    }
    let mut end = host.len();
    loop {
        let start = host[..end].rfind('.').map_or(0, |i| i + 1);
        let candidate = &host[start..];
        if !is_public_suffix(candidate) || start == 0 {
            return candidate;
        }
        end = start - 1;
    }
}

/// RFC 6265 domain matching: `a.example.com` matches `example.com`; IP
/// addresses match only themselves.
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (!is_ip(host)
            && host
                .strip_suffix(domain)
                .is_some_and(|rest| rest.ends_with('.')))
}

/// Where a request goes and who started it, for cookie decisions.
struct Context<'a> {
    target: &'a Origin,
    path: &'a str,
    /// Started by the user or a same-site page.
    same_site: bool,
    /// A top-level navigation (link, form submission), not a `fetch`.
    navigation: bool,
    safe_method: bool,
}

/// What [`TestClient::with_cookies`] keeps.
#[derive(Default)]
struct CookieStore {
    cookies: Vec<StoredCookie>,
    /// Creation counter: equal-length paths are sent oldest first.
    next: u64,
}

struct StoredCookie {
    cookie: JarCookie,
    created: u64,
}

impl StoredCookie {
    fn expired(&self, now: SystemTime) -> bool {
        self.cookie.expires.is_some_and(|at| at <= now)
    }

    /// Whether a request in `cx` carries this cookie.
    fn sent_with(&self, cx: &Context) -> bool {
        let c = &self.cookie;
        let host = &cx.target.host;
        let domain_ok = if c.host_only {
            *host == c.domain
        } else {
            domain_matches(host, &c.domain)
        };
        domain_ok
            && path_matches(cx.path, &c.path)
            && (!c.secure || cx.target.is_secure())
            && (cx.same_site
                || match c.same_site {
                    Some(CookieSameSite::None) => true,
                    Some(CookieSameSite::Strict) => false,
                    Some(CookieSameSite::Lax) | None => cx.navigation && cx.safe_method,
                })
    }
}

/// Bytes allowed in a cookie's name plus value (as browsers enforce).
const MAX_COOKIE_SIZE: usize = 4096;

impl CookieStore {
    /// Apply a response's `Set-Cookie` headers.
    fn store(&mut self, cx: &Context, headers: &HeaderMap) {
        for set in headers.get_all(SET_COOKIE) {
            if let Ok(set) = set.to_str() {
                self.set(cx, set);
            }
        }
    }

    /// Process one `Set-Cookie` the way browsers do (RFC 6265 section 5.3,
    /// with the `SameSite`, `Secure` and prefix rules of RFC 6265bis).
    /// Returns whether the store accepted it.
    fn set(&mut self, cx: &Context, header: &str) -> bool {
        let now = SystemTime::now();
        let Some(c) = SetCookie::parse(header) else {
            return false;
        };
        if c.name().len() + c.value().len() > MAX_COOKIE_SIZE {
            return false;
        }
        let host = &cx.target.host;
        let (domain, host_only) = match c.domain().map(|d| d.trim_start_matches('.')) {
            Some(d) if !d.is_empty() => {
                let d = d.to_ascii_lowercase();
                if d == *host {
                    (d, false)
                } else if is_public_suffix(&d) || !domain_matches(host, &d) {
                    // A cookie for `com`, or for another site entirely.
                    return false;
                } else {
                    (d, false)
                }
            }
            _ => (host.clone(), true),
        };
        let path = match c.path() {
            Some(p) if p.starts_with('/') => p.to_owned(),
            _ => default_path(cx.path).to_owned(),
        };
        let secure = c.secure();
        let secure_origin = cx.target.is_secure();
        if secure && !secure_origin {
            return false;
        }
        let same_site = c
            .same_site()
            .and_then(|v| match v.to_ascii_lowercase().as_str() {
                "strict" => Some(CookieSameSite::Strict),
                "lax" => Some(CookieSameSite::Lax),
                "none" => Some(CookieSameSite::None),
                _ => None,
            });
        if same_site == Some(CookieSameSite::None) && !secure {
            return false;
        }
        // Cross-site subresources can't set same-site cookies; cross-site
        // top-level navigations can.
        if !cx.same_site && same_site != Some(CookieSameSite::None) && !cx.navigation {
            return false;
        }
        let name = c.name();
        if starts_with_ignore_case(name, "__Secure-") && !secure {
            return false;
        }
        if starts_with_ignore_case(name, "__Host-")
            && !(secure && host_only && c.path() == Some("/"))
        {
            return false;
        }
        // An insecure origin can't overwrite (or shadow) a `Secure` cookie.
        if !secure_origin
            && self.cookies.iter().any(|s| {
                s.cookie.secure
                    && s.cookie.name == name
                    && (domain_matches(&s.cookie.domain, &domain)
                        || domain_matches(&domain, &s.cookie.domain))
                    && path_matches(&path, &s.cookie.path)
            })
        {
            return false;
        }
        let slot = self.cookies.iter().position(|s| {
            s.cookie.name == name && s.cookie.domain == domain && s.cookie.path == path
        });
        let expires = match c.expiry(now) {
            Expiry::Expired => {
                if let Some(i) = slot {
                    self.cookies.remove(i);
                }
                return true;
            }
            Expiry::At(at) => Some(at),
            Expiry::Session => None,
        };
        let cookie = JarCookie {
            name: name.to_owned(),
            value: c.value().to_owned(),
            domain,
            host_only,
            path,
            secure,
            http_only: c.http_only(),
            same_site,
            expires,
        };
        match slot {
            Some(i) => self.cookies[i].cookie = cookie,
            None => {
                self.next += 1;
                self.cookies.push(StoredCookie {
                    cookie,
                    created: self.next,
                });
            }
        }
        true
    }

    /// The unexpired cookies a request in `cx` carries: longest path first,
    /// then oldest first (RFC 6265 section 5.4).
    fn matching(&mut self, cx: &Context, scripts: bool) -> Vec<&JarCookie> {
        let now = SystemTime::now();
        self.cookies.retain(|c| !c.expired(now));
        let mut matching: Vec<&StoredCookie> = self
            .cookies
            .iter()
            .filter(|c| c.sent_with(cx) && !(scripts && c.cookie.http_only))
            .collect();
        matching.sort_by_key(|c| (std::cmp::Reverse(c.cookie.path.len()), c.created));
        matching.into_iter().map(|c| &c.cookie).collect()
    }

    /// Send the matching cookies in a single `Cookie` header after any the
    /// test set explicitly.
    fn add_cookie_header(&mut self, cx: &Context, headers: &mut HeaderMap) {
        let matching = self.matching(cx, false);
        if matching.is_empty() {
            return;
        }
        let explicit: Vec<String> = headers
            .get_all(COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        let explicit_names: Vec<&str> = explicit
            .iter()
            .flat_map(|h| h.split(';'))
            .filter_map(|pair| Some(pair.split_once('=')?.0.trim()))
            .collect();
        let mut header = explicit.join("; ");
        for c in matching {
            if explicit_names.contains(&c.name.as_str()) {
                continue;
            }
            if !header.is_empty() {
                header.push_str("; ");
            }
            header.push_str(&c.name);
            header.push('=');
            header.push_str(&c.value);
        }
        if let Ok(value) = HeaderValue::try_from(header) {
            headers.insert(COOKIE, value);
        }
    }
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The origin and path of an absolute URL given to a cookie helper.
#[track_caller]
fn page(url: &str) -> (Origin, String) {
    let uri: http::Uri = url
        .parse()
        .unwrap_or_else(|_| panic!("invalid URL: {url:?}"));
    let origin = Origin::from_uri(&uri)
        .unwrap_or_else(|| panic!("expected an absolute http(s) URL, got {url:?}"));
    let path = match uri.path() {
        "" => "/".to_owned(),
        p => p.to_owned(),
    };
    (origin, path)
}

/// The directory of the request path (RFC 6265, section 5.1.4).
fn default_path(request_path: &str) -> &str {
    match request_path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &request_path[..i],
    }
}

/// RFC 6265 path matching: `/admin` matches `/admin` and `/admin/x`, not
/// `/administrator`.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    match request_path.strip_prefix(cookie_path) {
        Some(rest) => rest.is_empty() || cookie_path.ends_with('/') || rest.starts_with('/'),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Multipart

/// A `multipart/form-data` body for [`TestRequest::multipart`].
///
/// ```
/// use zapline::test::MultipartForm;
///
/// let form = MultipartForm::new()
///     .text("title", "Holiday")
///     .file("photo", "beach.png", "image/png", vec![0x89, b'P', b'N', b'G']);
/// ```
#[derive(Clone, Debug, Default)]
pub struct MultipartForm {
    parts: Vec<Part>,
}

#[derive(Clone, Debug)]
struct Part {
    name: String,
    /// File name and content type.
    file: Option<(String, String)>,
    data: Bytes,
}

impl MultipartForm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a text field.
    pub fn text(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.parts.push(Part {
            name: name.into(),
            file: None,
            data: Bytes::from(value.into()),
        });
        self
    }

    /// Add a file field.
    pub fn file(
        mut self,
        name: impl Into<String>,
        file_name: impl Into<String>,
        content_type: impl Into<String>,
        data: impl Into<Bytes>,
    ) -> Self {
        self.parts.push(Part {
            name: name.into(),
            file: Some((file_name.into(), content_type.into())),
            data: data.into(),
        });
        self
    }

    /// The `Content-Type` (with a boundary found in no part) and the body.
    fn encode(&self) -> (String, Vec<u8>) {
        let boundary = loop {
            let b: String = std::iter::repeat_with(fastrand::alphanumeric)
                .take(24)
                .collect();
            let b = format!("zapline-{b}");
            if !self.parts.iter().any(|p| contains(&p.data, b.as_bytes())) {
                break b;
            }
        };
        let mut body = Vec::new();
        for part in &self.parts {
            body.extend_from_slice(b"--");
            body.extend_from_slice(boundary.as_bytes());
            body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
            body.extend_from_slice(escape(&part.name).as_bytes());
            body.push(b'"');
            if let Some((file_name, content_type)) = &part.file {
                body.extend_from_slice(b"; filename=\"");
                body.extend_from_slice(escape(file_name).as_bytes());
                body.extend_from_slice(b"\"\r\nContent-Type: ");
                body.extend_from_slice(content_type.as_bytes());
            }
            body.extend_from_slice(b"\r\n\r\n");
            body.extend_from_slice(&part.data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"--\r\n");
        (format!("multipart/form-data; boundary={boundary}"), body)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Quote-safe field and file names, encoded as browsers do.
fn escape(name: &str) -> String {
    name.replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_paths() {
        assert_eq!(default_path("/"), "/");
        assert_eq!(default_path("/login"), "/");
        assert_eq!(default_path("/account/login"), "/account");
        assert_eq!(default_path("/a/b/"), "/a/b");
        assert_eq!(default_path(""), "/");

        assert!(path_matches("/", "/"));
        assert!(path_matches("/admin", "/admin"));
        assert!(path_matches("/admin/users", "/admin"));
        assert!(path_matches("/admin/users", "/admin/"));
        assert!(!path_matches("/administrator", "/admin"));
        assert!(!path_matches("/", "/admin"));
        assert!(path_matches("/anything", "/"));
    }

    #[test]
    fn sites_and_domains() {
        assert_eq!(registrable_domain("app.example.com"), "example.com");
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("a.b.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("me.github.io"), "me.github.io");
        assert_eq!(registrable_domain("localhost"), "localhost");
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
        assert_eq!(registrable_domain("co.uk"), "co.uk");

        assert!(domain_matches("a.example.com", "example.com"));
        assert!(domain_matches("example.com", "example.com"));
        assert!(!domain_matches("badexample.com", "example.com"));
        assert!(!domain_matches("example.com", "a.example.com"));
        assert!(!domain_matches("1.2.3.4", "2.3.4"));

        let https = |host: &str| Origin {
            https: true,
            host: host.into(),
            port: None,
        };
        assert!(https("a.example.com").same_site(&https("b.example.com")));
        assert!(!https("a.github.io").same_site(&https("b.github.io")));
        let http = Origin {
            https: false,
            ..https("example.com")
        };
        assert!(!http.same_site(&https("example.com")), "schemeful");
        assert!(Origin::localhost().is_secure());
        assert!(!http.is_secure());
        let with_port = Origin {
            port: Some(443),
            ..https("example.com")
        };
        assert!(with_port.same_origin(&https("example.com")));
        assert_eq!(
            Origin {
                port: Some(8443),
                ..https("::1")
            }
            .serialize(),
            "https://[::1]:8443"
        );
    }

    #[test]
    fn set_cookie_parsing() {
        let c = SetCookie::parse("sid=a=b; Path=/x; max-age=60; HttpOnly; SameSite=Lax").unwrap();
        assert_eq!((c.name(), c.value()), ("sid", "a=b"));
        assert_eq!(c.path(), Some("/x"));
        assert_eq!(c.max_age(), Some(60));
        assert_eq!(c.same_site(), Some("Lax"));
        assert!(c.http_only() && !c.secure());
        assert!(!c.is_removal());
        assert!(SetCookie::parse("x=; Max-Age=0").unwrap().is_removal());
        assert!(
            SetCookie::parse("x=; Expires=Thu, 01 Jan 1970 00:00:00 GMT")
                .unwrap()
                .is_removal()
        );
        // Max-Age wins over Expires.
        assert!(
            !SetCookie::parse("x=1; Max-Age=60; Expires=Thu, 01 Jan 1970 00:00:00 GMT")
                .unwrap()
                .is_removal()
        );
        // Last attribute wins.
        assert_eq!(
            SetCookie::parse("x=1; Path=/a; Path=/b").unwrap().path(),
            Some("/b")
        );
        assert!(SetCookie::parse("novalue").is_none());
        assert!(SetCookie::parse("=v").is_none());
    }

    #[test]
    fn multipart_boundary_avoids_content() {
        let form = MultipartForm::new()
            .text("a", "x")
            .file("f", "a\"b.txt", "text/plain", "y");
        let (ct, body) = form.encode();
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(body.starts_with(&format!("--{boundary}\r\n")));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
        assert!(body.contains(
            "name=\"f\"; filename=\"a%22b.txt\"\r\nContent-Type: text/plain\r\n\r\ny\r\n"
        ));
    }
}
