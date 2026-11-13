//! Calling other services over HTTP(S) (feature `http-client`).
//!
//! [`HttpClient`] is a small client with production defaults: timeouts,
//! retries with backoff for idempotent requests, a cap on buffered response
//! bodies, and a guard against server-side request forgery. Addresses on
//! private, loopback, link-local (cloud metadata, `169.254.169.254`) and
//! other reserved networks are refused unless
//! [`allow_private_networks`](HttpClient::allow_private_networks) says
//! otherwise, and redirects are never followed, so a URL a user typed can't
//! reach your internal network.
//!
//! ```no_run
//! use serde::Deserialize;
//! use zapline::outbound::HttpClient;
//! use zapline::prelude::*;
//!
//! #[derive(Deserialize, serde::Serialize)]
//! struct Rate { usd: f64 }
//!
//! async fn rate(http: HttpClient) -> Result<Json<Rate>> {
//!     let rate: Rate = http
//!         .get("https://api.example.com/rate")
//!         .bearer(&std::env::var("RATES_TOKEN").unwrap_or_default())
//!         .send()
//!         .await?
//!         .error_for_status()
//!         .await?
//!         .json()
//!         .await?;
//!     Ok(Json(rate))
//! }
//!
//! let app = App::new().get("/rate", rate);
//! ```
//!
//! Errors are [`Error`]s a handler can return with `?`: `502` (code
//! `upstream_unreachable` or `upstream_status`), `504` (`upstream_timeout`)
//! and `400` (`upstream_blocked`, for a refused address).

use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http::header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER, USER_AGENT};
use http::{HeaderMap, Method, Request as HttpRequest, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::extract::FromRequestParts;
use crate::request::Request;
use crate::{Error, Result};

type Hyper = Client<HttpsConnector<HttpConnector<GuardedResolver>>, Full<Bytes>>;

/// An HTTP(S) client with timeouts, retries and an SSRF guard. Cheap to
/// clone (clones share connections). As a handler argument it is the one
/// registered with `App::state`, or a shared default.
#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<Inner>,
}

struct Inner {
    hyper: Hyper,
    config: Config,
}

#[derive(Clone)]
struct Config {
    timeout: Duration,
    connect_timeout: Duration,
    retries: u32,
    max_response_size: usize,
    allow_private: bool,
    headers: HeaderMap,
}

impl Default for Config {
    fn default() -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("zapline/", env!("CARGO_PKG_VERSION"))),
        );
        Config {
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            retries: 2,
            max_response_size: 16 * 1024 * 1024,
            allow_private: false,
            headers,
        }
    }
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = &self.inner.config;
        f.debug_struct("HttpClient")
            .field("timeout", &c.timeout)
            .field("retries", &c.retries)
            .field("allow_private_networks", &c.allow_private)
            .finish_non_exhaustive()
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    /// A client with the defaults: 30 s timeout, 10 s to connect, 2 retries
    /// for idempotent requests, responses buffered up to 16 MiB, private
    /// networks refused.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let http = HttpClient::new();
    /// ```
    pub fn new() -> Self {
        Self::build(Config::default())
    }

    fn build(config: Config) -> Self {
        let mut http = HttpConnector::new_with_resolver(GuardedResolver {
            allow_private: config.allow_private,
        });
        http.enforce_http(false);
        http.set_connect_timeout(Some(config.connect_timeout));
        http.set_nodelay(true);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let hyper = Client::builder(TokioExecutor::new()).build(https);
        HttpClient {
            inner: Arc::new(Inner { hyper, config }),
        }
    }

    fn with(self, f: impl FnOnce(&mut Config)) -> Self {
        let mut config = self.inner.config.clone();
        f(&mut config);
        Self::build(config)
    }

    /// The longest a request may take: until the response headers arrive,
    /// and again for reading a buffered body (`bytes`, `text`, `json`), or
    /// between two chunks when streaming. Default 30 s.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::outbound::HttpClient;
    /// let http = HttpClient::new().timeout(Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn timeout(self, timeout: Duration) -> Self {
        self.with(|c| c.timeout = timeout)
    }

    /// The longest opening a connection may take. Default 10 s.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::outbound::HttpClient;
    /// let http = HttpClient::new().connect_timeout(Duration::from_secs(2));
    /// ```
    #[must_use]
    pub fn connect_timeout(self, timeout: Duration) -> Self {
        self.with(|c| c.connect_timeout = timeout)
    }

    /// How many times an idempotent request (`GET`, `HEAD`, `PUT`, `DELETE`,
    /// `OPTIONS`) is retried after a connection failure, a timeout, or a
    /// `429`, `502`, `503` or `504`. Waits 100 ms, then 200 ms, ... (with
    /// jitter), or what `Retry-After` asks for, up to 10 s. Default 2; `POST`
    /// and `PATCH` are never retried.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let http = HttpClient::new().retries(0);
    /// ```
    #[must_use]
    pub fn retries(self, n: u32) -> Self {
        self.with(|c| c.retries = n)
    }

    /// The largest response body `bytes`, `text` and `json` will buffer
    /// (default 16 MiB); a larger one fails with `502`. Streaming with
    /// [`HttpResponse::chunk`] is not limited.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let http = HttpClient::new().max_response_size(1024 * 1024);
    /// ```
    #[must_use]
    pub fn max_response_size(self, bytes: usize) -> Self {
        self.with(|c| c.max_response_size = bytes)
    }

    /// Allow addresses on private, loopback, link-local and other reserved
    /// networks. Off by default, so a URL from user input can't reach
    /// internal services or the cloud metadata endpoint. Turn it on only for
    /// a client that talks to your own internal services, never for URLs
    /// users supply.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let internal = HttpClient::new().allow_private_networks(true);
    /// ```
    #[must_use]
    pub fn allow_private_networks(self, allow: bool) -> Self {
        self.with(|c| c.allow_private = allow)
    }

    /// A header sent with every request (an API key, a custom
    /// `User-Agent`).
    ///
    /// # Panics
    ///
    /// If `name` or `value` is not a valid header.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let http = HttpClient::new().header("x-api-key", "k-123");
    /// ```
    #[must_use]
    pub fn header(self, name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("a valid header name");
        let value = HeaderValue::from_str(value).expect("a valid header value");
        self.with(|c| {
            c.headers.insert(name, value);
        })
    }

    /// Start a request.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// use zapline::Method;
    /// let req = HttpClient::new().request(Method::OPTIONS, "https://example.com/");
    /// ```
    pub fn request(&self, method: Method, url: &str) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            url: url.to_owned(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            timeout: None,
            error: None,
        }
    }

    /// Start a `GET`.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().get("https://example.com/");
    /// ```
    pub fn get(&self, url: &str) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// Start a `POST`.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().post("https://example.com/hooks");
    /// ```
    pub fn post(&self, url: &str) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// Start a `PUT`.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().put("https://example.com/items/1");
    /// ```
    pub fn put(&self, url: &str) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// Start a `PATCH`.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().patch("https://example.com/items/1");
    /// ```
    pub fn patch(&self, url: &str) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    /// Start a `DELETE`.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().delete("https://example.com/items/1");
    /// ```
    pub fn delete(&self, url: &str) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }
}

impl FromRequestParts for HttpClient {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        static SHARED: OnceLock<HttpClient> = OnceLock::new();
        Ok(req
            .state::<HttpClient>()
            .cloned()
            .unwrap_or_else(|| SHARED.get_or_init(HttpClient::new).clone()))
    }
}

/// A request being built; [`send`](Self::send) it.
#[must_use = "a request does nothing until it is sent"]
pub struct RequestBuilder {
    client: HttpClient,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Bytes,
    timeout: Option<Duration>,
    error: Option<Error>,
}

impl fmt::Debug for RequestBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestBuilder")
            .field("method", &self.method)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl RequestBuilder {
    /// Add a header. An invalid name or value fails the request when sent.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().get("https://example.com/").header("accept", "text/csv");
    /// ```
    pub fn header(mut self, name: &str, value: &str) -> Self {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(n), Ok(v)) => {
                self.headers.insert(n, v);
            }
            _ => {
                self.error.get_or_insert_with(|| {
                    Error::internal(format!("invalid header `{name}` for an outbound request"))
                });
            }
        }
        self
    }

    /// `Authorization: Bearer <token>`.
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().get("https://example.com/").bearer("t0k3n");
    /// ```
    pub fn bearer(self, token: &str) -> Self {
        self.header(AUTHORIZATION.as_str(), &format!("Bearer {token}"))
    }

    /// A JSON body (and `Content-Type: application/json`).
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new()
    ///     .post("https://example.com/hooks")
    ///     .json(&serde_json::json!({"event": "signup"}));
    /// ```
    pub fn json<T: Serialize + ?Sized>(mut self, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => {
                self.body = Bytes::from(body);
                self.headers
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }
            Err(e) => {
                self.error.get_or_insert_with(|| {
                    Error::internal("cannot serialize an outbound JSON body").with_source(e)
                });
            }
        }
        self
    }

    /// A raw body; set `Content-Type` with [`header`](Self::header).
    ///
    /// ```
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new()
    ///     .post("https://example.com/upload")
    ///     .header("content-type", "text/plain")
    ///     .body("hello");
    /// ```
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// This request's [timeout](HttpClient::timeout) instead of the
    /// client's.
    ///
    /// ```
    /// use std::time::Duration;
    /// use zapline::outbound::HttpClient;
    /// let req = HttpClient::new().get("https://example.com/").timeout(Duration::from_secs(2));
    /// ```
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Send it, retrying idempotent requests as [`HttpClient::retries`]
    /// says. Any status is `Ok`; use [`HttpResponse::error_for_status`] to
    /// turn non-2xx answers into errors.
    ///
    /// # Errors
    ///
    /// `400 upstream_blocked` for a refused address, `502
    /// upstream_unreachable` when no connection could be made, `504
    /// upstream_timeout` after the timeout.
    ///
    /// ```no_run
    /// # async fn run() -> zapline::Result<()> {
    /// use zapline::outbound::HttpClient;
    /// let res = HttpClient::new().get("https://example.com/").send().await?;
    /// println!("{}", res.status());
    /// # Ok(()) }
    /// ```
    pub async fn send(self) -> Result<HttpResponse> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let config = &self.client.inner.config;
        let uri: Uri = self.url.parse().map_err(|e| {
            Error::bad_request(format!("invalid URL `{}`", self.url))
                .with_code("upstream_invalid_url")
                .with_source(e)
        })?;
        let host = match (uri.scheme_str(), uri.host()) {
            (Some("http" | "https"), Some(host)) => {
                host.trim_start_matches('[').trim_end_matches(']')
            }
            _ => {
                return Err(Error::bad_request(format!(
                    "only absolute http(s) URLs can be fetched, not `{}`",
                    self.url
                ))
                .with_code("upstream_invalid_url"));
            }
        };
        // Literal IPs skip the resolver, so they are checked here.
        if let Ok(ip) = host.parse::<IpAddr>() {
            if !config.allow_private && !is_public(ip) {
                return Err(blocked(host));
            }
        }
        let timeout = self.timeout.unwrap_or(config.timeout);
        let retries = if is_idempotent(&self.method) {
            config.retries
        } else {
            0
        };
        let mut headers = config.headers.clone();
        headers.extend(self.headers);
        let mut attempt = 0;
        loop {
            let mut req = HttpRequest::builder()
                .method(self.method.clone())
                .uri(uri.clone())
                .body(Full::new(self.body.clone()))
                .map_err(|e| Error::internal("invalid outbound request").with_source(e))?;
            *req.headers_mut() = headers.clone();
            let outcome = tokio::time::timeout(timeout, self.client.inner.hyper.request(req)).await;
            let wait = match &outcome {
                Ok(Ok(res)) if attempt < retries && is_retryable(res.status()) => {
                    Some(retry_after(res.headers()).unwrap_or_else(|| backoff(attempt)))
                }
                Ok(Err(e)) if attempt < retries && !is_blocked(e) => Some(backoff(attempt)),
                Err(_) if attempt < retries => Some(backoff(attempt)),
                _ => None,
            };
            if let Some(wait) = wait {
                attempt += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            return match outcome {
                Ok(Ok(res)) => Ok(HttpResponse {
                    status: res.status(),
                    headers: res.headers().clone(),
                    body: res.into_body(),
                    timeout,
                    max: config.max_response_size,
                    url: self.url,
                }),
                Ok(Err(e)) if is_blocked(&e) => Err(blocked(host)),
                Ok(Err(e)) => Err(Error::new(
                    StatusCode::BAD_GATEWAY,
                    format!("cannot reach {host}"),
                )
                .with_code("upstream_unreachable")
                .with_source(e)),
                Err(_) => Err(upstream_timeout(host)),
            };
        }
    }
}

/// A response: status and headers arrived, the body is read on demand.
pub struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: hyper::body::Incoming,
    timeout: Duration,
    max: usize,
    url: String,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    /// The status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// One header as text, if present and valid UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// `Ok(self)` for a 2xx status. Otherwise a `502` error (code
    /// `upstream_status`) naming the status and the start of the body.
    ///
    /// # Errors
    ///
    /// For any status outside 200–299.
    pub async fn error_for_status(self) -> Result<Self> {
        if self.status.is_success() {
            return Ok(self);
        }
        let (status, url) = (self.status, self.url.clone());
        let mut res = self;
        res.max = 2048;
        let detail = match res.bytes_lossy().await {
            Some(body) if !body.trim().is_empty() => format!(": {}", body.trim()),
            _ => String::new(),
        };
        Err(Error::new(
            StatusCode::BAD_GATEWAY,
            format!("{url} answered {status}{detail}"),
        )
        .with_code("upstream_status"))
    }

    async fn bytes_lossy(mut self) -> Option<String> {
        let mut out = BytesMut::new();
        while let Ok(Some(chunk)) = self.chunk().await {
            let room = self.max.saturating_sub(out.len());
            out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if out.len() >= self.max {
                break;
            }
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }

    /// The whole body, within the client's size limit and timeout.
    ///
    /// # Errors
    ///
    /// `502` when the body is larger than
    /// [`max_response_size`](HttpClient::max_response_size) or the
    /// connection fails, `504` after the timeout.
    pub async fn bytes(self) -> Result<Bytes> {
        let host = host_of(&self.url);
        let (timeout, max) = (self.timeout, self.max);
        let limited = http_body_util::Limited::new(self.body, max);
        match tokio::time::timeout(timeout, limited.collect()).await {
            Err(_) => Err(upstream_timeout(&host)),
            Ok(Ok(collected)) => Ok(collected.to_bytes()),
            Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => Err(Error::new(
                StatusCode::BAD_GATEWAY,
                format!("the response from {host} is larger than {max} bytes"),
            )
            .with_code("upstream_too_large")),
            Ok(Err(e)) => Err(Error::new(
                StatusCode::BAD_GATEWAY,
                format!("reading the response from {host} failed"),
            )
            .with_code("upstream_unreachable")
            .with_source(io::Error::other(e.to_string()))),
        }
    }

    /// The body as text (invalid UTF-8 is replaced).
    ///
    /// # Errors
    ///
    /// As [`bytes`](Self::bytes).
    pub async fn text(self) -> Result<String> {
        let bytes = self.bytes().await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The body parsed as JSON.
    ///
    /// # Errors
    ///
    /// As [`bytes`](Self::bytes), and `502 upstream_invalid_json` when it
    /// doesn't parse as `T`.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        let host = host_of(&self.url);
        let bytes = self.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::new(
                StatusCode::BAD_GATEWAY,
                format!("{host} answered JSON that doesn't match the expected shape"),
            )
            .with_code("upstream_invalid_json")
            .with_source(e)
        })
    }

    /// The next chunk of the body as it arrives, or `None` at the end: for
    /// streams (server-sent events, LLM tokens, large downloads). Each wait
    /// is bounded by the timeout; the total is not.
    ///
    /// # Errors
    ///
    /// `502` when the connection fails, `504` when no data arrives within
    /// the timeout.
    ///
    /// ```no_run
    /// # async fn run() -> zapline::Result<()> {
    /// use zapline::outbound::HttpClient;
    /// let mut res = HttpClient::new().get("https://example.com/events").send().await?;
    /// while let Some(chunk) = res.chunk().await? {
    ///     print!("{}", String::from_utf8_lossy(&chunk));
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        loop {
            let frame = match tokio::time::timeout(self.timeout, self.body.frame()).await {
                Err(_) => return Err(upstream_timeout(&host_of(&self.url))),
                Ok(None) => return Ok(None),
                Ok(Some(Err(e))) => {
                    return Err(Error::new(
                        StatusCode::BAD_GATEWAY,
                        format!("reading the response from {} failed", host_of(&self.url)),
                    )
                    .with_code("upstream_unreachable")
                    .with_source(e))
                }
                Ok(Some(Ok(frame))) => frame,
            };
            if let Ok(data) = frame.into_data() {
                if !data.is_empty() {
                    return Ok(Some(data));
                }
            }
        }
    }
}

fn host_of(url: &str) -> String {
    url.parse::<Uri>()
        .ok()
        .and_then(|u| u.host().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

fn blocked(host: &str) -> Error {
    Error::bad_request(format!(
        "{host} is on a private or reserved network, which outbound requests may not reach"
    ))
    .with_code("upstream_blocked")
}

fn upstream_timeout(host: &str) -> Error {
    Error::new(
        StatusCode::GATEWAY_TIMEOUT,
        format!("{host} did not answer in time"),
    )
    .with_code("upstream_timeout")
}

fn is_idempotent(m: &Method) -> bool {
    matches!(
        *m,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS | Method::TRACE
    )
}

fn is_retryable(s: StatusCode) -> bool {
    matches!(s.as_u16(), 429 | 502 | 503 | 504)
}

/// 100 ms × 2^attempt, ± 25 %.
fn backoff(attempt: u32) -> Duration {
    let base = 100u64.saturating_mul(1 << attempt.min(6));
    let jitter = fastrand::u64(0..=base / 2);
    Duration::from_millis(base - base / 4 + jitter)
}

/// `Retry-After: <seconds>`, capped at 10 s.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(10)))
}

/// Whether the connection failed because the resolver refused the address.
fn is_blocked(e: &hyper_util::client::legacy::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            if io.kind() == io::ErrorKind::PermissionDenied && io.to_string().contains(BLOCKED_MARK)
            {
                return true;
            }
        }
        source = err.source();
    }
    false
}

const BLOCKED_MARK: &str = "outbound requests may not reach";

/// Whether outbound requests may reach `ip` by default: not loopback,
/// private, link-local, shared (CGNAT), documentation, benchmarking,
/// multicast, broadcast or otherwise reserved.
///
/// ```
/// use zapline::outbound::is_public;
/// assert!(is_public("93.184.216.34".parse().unwrap()));
/// assert!(!is_public("169.254.169.254".parse().unwrap())); // cloud metadata
/// assert!(!is_public("10.0.0.1".parse().unwrap()));
/// assert!(!is_public("::ffff:127.0.0.1".parse().unwrap()));
/// ```
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64: may map to a private IPv4
        || (s[0] == 0x2002)) // 6to4: may embed a private IPv4
}

/// A DNS resolver that drops refused addresses, so a hostname pointing at
/// an internal address (or rebinding to one) can't be reached.
#[derive(Clone)]
struct GuardedResolver {
    allow_private: bool,
}

type Resolving = Pin<Box<dyn Future<Output = io::Result<std::vec::IntoIter<SocketAddr>>> + Send>>;

impl tower_service::Service<Name> for GuardedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Resolving;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let allow = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = found
                .into_iter()
                .filter(|a| allow || is_public(a.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{host} resolves only to addresses {BLOCKED_MARK}"),
                ));
            }
            Ok(allowed.into_iter())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "2001:db8::1",
            "64:ff9b::a00:1",
            "2002:a00:1::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "93.184.216.34",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn backoff_grows_and_retry_after_is_capped() {
        assert!(backoff(0) <= Duration::from_millis(125));
        assert!(backoff(3) >= Duration::from_millis(600));
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(10)));
    }
}
