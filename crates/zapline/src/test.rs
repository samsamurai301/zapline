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

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
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

/// In-process client. Cheap to clone; clones share one app instance.
#[derive(Clone)]
pub struct TestClient {
    inner: Arc<Inner>,
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
        }
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

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    #[cfg(feature = "json")]
    pub fn json<T: serde::Serialize>(self, value: &T) -> Self {
        let body = serde_json::to_vec(value).expect("failed to serialize test JSON body");
        self.header(http::header::CONTENT_TYPE, "application/json")
            .body(body)
    }

    #[cfg(feature = "form")]
    pub fn form<T: serde::Serialize>(self, value: &T) -> Self {
        let body = serde_urlencoded::to_string(value).expect("failed to serialize test form");
        self.header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
    }

    pub async fn send(self) -> TestResponse {
        let req = self
            .builder
            .body(ReqBody::Full(self.body))
            .expect("invalid test request");
        let started = self.client.started().await;
        let res = started
            .app
            .handle(req, self.conn)
            .await
            .unwrap_or_else(|e| match e {});
        let (parts, body) = res.into_parts();
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
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
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
}
