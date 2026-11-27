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
//!   browser, for login and session flows.
//! - **Assertions**: [`TestResponse::assert_status`] and
//!   [`assert_header`](TestResponse::assert_header) chain, and their panic
//!   messages include the response body.
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
use http::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt;

use crate::app::{AppInner, Prepared};
use crate::body::ReqBody;
use crate::lifecycle::{Lifecycle, Resources};
use crate::request::ConnInfo;
use crate::router::RouteInfo;

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
        }
    }

    /// This client with a fresh, empty cookie store, shared by its clones:
    /// like a browser, it stores every `Set-Cookie` it receives and sends
    /// matching cookies back in one `Cookie` header.
    ///
    /// The store honors `Path` (prefix matching, defaulting to the request's
    /// directory), `Max-Age` and `Expires`; a cookie with `Max-Age=0` or a
    /// past `Expires` is deleted. All requests count as same-site and
    /// secure, so `Domain`, `Secure` and `SameSite` are ignored. Cookies set
    /// explicitly with [`TestRequest::header`] are sent first and take
    /// precedence over stored ones with the same name.
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
            .find(|c| c.name == name && !c.expired(now))
            .map(|c| c.value.clone())
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
        let mut req = self
            .builder
            .body(ReqBody::Full(self.body))
            .expect("invalid test request");
        let jar = self.client.cookies.as_ref().map(|jar| {
            let path = req.uri().path().to_owned();
            jar.lock()
                .unwrap()
                .add_cookie_header(&path, req.headers_mut());
            (jar, path)
        });
        let started = self.client.started().await;
        let res = started
            .app
            .handle(req, self.conn)
            .await
            .unwrap_or_else(|e| match e {});
        let (parts, body) = res.into_parts();
        if let Some((jar, path)) = jar {
            jar.lock().unwrap().store(&path, &parts.headers);
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

/// What [`TestClient::with_cookies`] keeps: cookies by (name, path).
#[derive(Default)]
struct CookieStore {
    cookies: Vec<StoredCookie>,
}

struct StoredCookie {
    name: String,
    value: String,
    path: String,
    expires: Option<SystemTime>,
}

impl StoredCookie {
    fn expired(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }
}

impl CookieStore {
    /// Apply a response's `Set-Cookie` headers.
    fn store(&mut self, request_path: &str, headers: &HeaderMap) {
        let now = SystemTime::now();
        for set in headers.get_all(SET_COOKIE) {
            let Some(c) = set.to_str().ok().and_then(SetCookie::parse) else {
                continue;
            };
            let path = match c.path() {
                Some(p) if p.starts_with('/') => p.to_owned(),
                _ => default_path(request_path).to_owned(),
            };
            let slot = self
                .cookies
                .iter()
                .position(|s| s.name == c.name() && s.path == path);
            let expires = match c.expiry(now) {
                Expiry::Expired => {
                    if let Some(i) = slot {
                        self.cookies.remove(i);
                    }
                    continue;
                }
                Expiry::At(at) => Some(at),
                Expiry::Session => None,
            };
            let cookie = StoredCookie {
                name: c.name().to_owned(),
                value: c.value().to_owned(),
                path,
                expires,
            };
            match slot {
                Some(i) => self.cookies[i] = cookie,
                None => self.cookies.push(cookie),
            }
        }
    }

    /// Send the cookies matching `request_path`, longest path first, in a
    /// single `Cookie` header after any the test set explicitly.
    fn add_cookie_header(&mut self, request_path: &str, headers: &mut HeaderMap) {
        let now = SystemTime::now();
        self.cookies.retain(|c| !c.expired(now));
        let mut matching: Vec<&StoredCookie> = self
            .cookies
            .iter()
            .filter(|c| path_matches(request_path, &c.path))
            .collect();
        if matching.is_empty() {
            return;
        }
        matching.sort_by_key(|c| std::cmp::Reverse(c.path.len()));

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
