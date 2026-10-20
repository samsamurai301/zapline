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

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt;

use crate::app::AppInner;
use crate::body::ReqBody;

#[derive(Clone)]
pub struct TestClient {
    app: Arc<AppInner>,
}

impl TestClient {
    pub(crate) fn new(app: Arc<AppInner>) -> Self {
        TestClient { app }
    }

    pub fn request(&self, method: Method, uri: &str) -> TestRequest {
        TestRequest {
            app: self.app.clone(),
            builder: http::Request::builder().method(method).uri(uri),
            body: Bytes::new(),
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
    app: Arc<AppInner>,
    builder: http::request::Builder,
    body: Bytes,
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
        let res = self.app.handle(req).await.unwrap_or_else(|e| match e {});
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
