//! The small HTTPS client behind S3 storage and OAuth logins: hyper with
//! rustls and the Mozilla root certificates, plain HTTP allowed (for local
//! S3-compatible servers and test doubles).

use std::sync::OnceLock;

use bytes::Bytes;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::{Error, Result};

type HttpsClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// A buffered response.
pub(crate) struct Fetched {
    pub(crate) status: StatusCode,
    #[allow(dead_code)]
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

fn client() -> &'static HttpsClient {
    static CLIENT: OnceLock<HttpsClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Client::builder(TokioExecutor::new()).build(https)
    })
}

/// Send `req` and buffer the response. `what` names the service in errors
/// ("S3", "the OAuth provider").
pub(crate) async fn fetch(req: Request<Full<Bytes>>, what: &str) -> Result<Fetched> {
    let res = client()
        .request(req)
        .await
        .map_err(|e| Error::service_unavailable(format!("cannot reach {what}")).with_source(e))?;
    let (parts, body) = res.into_parts();
    let body = body
        .collect()
        .await
        .map_err(|e| Error::service_unavailable(format!("{what} response failed")).with_source(e))?
        .to_bytes();
    Ok(Fetched {
        status: parts.status,
        headers: parts.headers,
        body,
    })
}

/// Send `req` and return the response with its body unread, for large
/// downloads (S3 `get_stream`).
#[cfg(feature = "s3")]
pub(crate) async fn fetch_streaming(
    req: Request<Full<Bytes>>,
    what: &str,
) -> Result<http::Response<hyper::body::Incoming>> {
    client()
        .request(req)
        .await
        .map_err(|e| Error::service_unavailable(format!("cannot reach {what}")).with_source(e))
}
