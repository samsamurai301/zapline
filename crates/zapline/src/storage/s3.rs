//! A minimal S3 client: PUT, GET, HEAD, DELETE and presigned GET links,
//! signed with AWS Signature Version 4.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hmac::{Hmac, Mac};
use http::{Method, Request as HttpRequest, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};

use super::{encode_path, hex};
use crate::{Error, Result};

/// The longest a presigned link may last (an S3 limit).
const MAX_PRESIGN: Duration = Duration::from_secs(7 * 24 * 3600);

/// S3 settings for [`Storage::s3`](super::Storage::s3). Works with AWS and
/// S3-compatible services (Cloudflare R2, MinIO, Backblaze B2, ...) through
/// [`endpoint`](Self::endpoint).
#[derive(Clone)]
pub struct S3 {
    bucket: String,
    region: String,
    /// `scheme://host[:port]`, without the bucket.
    endpoint: Option<String>,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl fmt::Debug for S3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key", &self.access_key)
            .finish_non_exhaustive()
    }
}

impl S3 {
    /// A bucket on AWS in `region` (e.g. `us-east-1`). Set the credentials
    /// with [`credentials`](Self::credentials).
    ///
    /// ```
    /// let s3 = zapline::storage::S3::new("my-bucket", "eu-west-1");
    /// ```
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        S3 {
            bucket: bucket.into(),
            region: region.into(),
            endpoint: None,
            access_key: String::new(),
            secret_key: String::new(),
            session_token: None,
            client: Client::builder(TokioExecutor::new()).build(https),
        }
    }

    /// Reads `S3_BUCKET`, `S3_REGION` (or `AWS_REGION`, default
    /// `us-east-1`), `S3_ENDPOINT` (for R2, MinIO, ...),
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally
    /// `AWS_SESSION_TOKEN`.
    ///
    /// # Errors
    ///
    /// When the bucket or the credentials are missing.
    ///
    /// ```no_run
    /// let s3 = zapline::storage::S3::from_env()?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn from_env() -> Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let need = |name: &str| {
            var(name).ok_or_else(|| Error::internal(format!("S3 storage needs {name}")))
        };
        let region = var("S3_REGION")
            .or_else(|| var("AWS_REGION"))
            .unwrap_or_else(|| "us-east-1".to_owned());
        let mut s3 = S3::new(need("S3_BUCKET")?, region)
            .credentials(need("AWS_ACCESS_KEY_ID")?, need("AWS_SECRET_ACCESS_KEY")?);
        if let Some(endpoint) = var("S3_ENDPOINT") {
            s3 = s3.endpoint(endpoint);
        }
        if let Some(token) = var("AWS_SESSION_TOKEN") {
            s3 = s3.session_token(token);
        }
        Ok(s3)
    }

    /// An S3-compatible service at `url` (e.g.
    /// `https://<account>.r2.cloudflarestorage.com` or
    /// `http://localhost:9000`), addressed path-style (`url/bucket/key`).
    ///
    /// ```
    /// let s3 = zapline::storage::S3::new("files", "auto").endpoint("http://localhost:9000");
    /// ```
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into().trim_end_matches('/').to_owned());
        self
    }

    /// The access key id and secret.
    ///
    /// ```
    /// let s3 = zapline::storage::S3::new("b", "us-east-1").credentials("AKIA...", "secret");
    /// ```
    pub fn credentials(
        mut self,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        self.access_key = access_key.into();
        self.secret_key = secret_key.into();
        self
    }

    /// A session token, for temporary credentials.
    ///
    /// ```
    /// let s3 = zapline::storage::S3::new("b", "us-east-1").session_token("...");
    /// ```
    pub fn session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }

    pub(super) fn bucket(&self) -> &str {
        &self.bucket
    }

    /// `(scheme, host, path)` of an object.
    fn locate(&self, key: &str) -> (&str, String, String) {
        let key = encode_path(key);
        match &self.endpoint {
            Some(endpoint) => {
                let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", endpoint));
                (
                    scheme,
                    host.to_owned(),
                    format!("/{}/{key}", encode_path(&self.bucket)),
                )
            }
            None if self.region == "us-east-1" => (
                "https",
                format!("{}.s3.amazonaws.com", self.bucket),
                format!("/{key}"),
            ),
            None => (
                "https",
                format!("{}.s3.{}.amazonaws.com", self.bucket, self.region),
                format!("/{key}"),
            ),
        }
    }

    pub(super) async fn put(&self, key: &str, body: Bytes, content_type: &str) -> Result<()> {
        let res = self
            .send(Method::PUT, key, body, Some(content_type))
            .await?;
        expect_ok(res, "store the file")
    }

    pub(super) async fn get(&self, key: &str) -> Result<Bytes> {
        let res = self.send(Method::GET, key, Bytes::new(), None).await?;
        match res.0 {
            StatusCode::NOT_FOUND => Err(Error::not_found()),
            _ => expect_ok_body(res, "read the file"),
        }
    }

    pub(super) async fn exists(&self, key: &str) -> Result<bool> {
        let res = self.send(Method::HEAD, key, Bytes::new(), None).await?;
        match res.0 {
            StatusCode::NOT_FOUND => Ok(false),
            _ => expect_ok(res, "check the file").map(|()| true),
        }
    }

    pub(super) async fn delete(&self, key: &str) -> Result<()> {
        let res = self.send(Method::DELETE, key, Bytes::new(), None).await?;
        expect_ok(res, "delete the file")
    }

    async fn send(
        &self,
        method: Method,
        key: &str,
        body: Bytes,
        content_type: Option<&str>,
    ) -> Result<(StatusCode, Bytes)> {
        let (scheme, host, path) = self.locate(key);
        let now = SystemTime::now();
        let payload_hash = hex(&Sha256::digest(&body));
        let mut headers = vec![
            ("host", host.clone()),
            ("x-amz-content-sha256", payload_hash.clone()),
            ("x-amz-date", amz_date(now)),
        ];
        if let Some(token) = &self.session_token {
            headers.push(("x-amz-security-token", token.clone()));
        }
        let auth = self.authorization(method.as_str(), &path, "", &mut headers, &payload_hash, now);

        let mut req = HttpRequest::builder()
            .method(method)
            .uri(format!("{scheme}://{host}{path}"));
        for (name, value) in &headers {
            if *name != "host" {
                req = req.header(*name, value);
            }
        }
        if let Some(content_type) = content_type {
            req = req.header(http::header::CONTENT_TYPE, content_type);
        }
        let req = req
            .header(http::header::AUTHORIZATION, auth)
            .body(Full::new(body))
            .map_err(|e| Error::internal("invalid S3 request").with_source(e))?;
        let res = self
            .client
            .request(req)
            .await
            .map_err(|e| Error::service_unavailable("cannot reach S3").with_source(e))?;
        let status = res.status();
        let body = res
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::service_unavailable("S3 response failed").with_source(e))?
            .to_bytes();
        Ok((status, body))
    }

    /// The `Authorization` header for a request. Sorts `headers`.
    fn authorization(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &mut [(&str, String)],
        payload_hash: &str,
        now: SystemTime,
    ) -> String {
        headers.sort_by(|a, b| a.0.cmp(b.0));
        let signed: Vec<&str> = headers.iter().map(|(n, _)| *n).collect();
        let signed = signed.join(";");
        let mut canonical = format!("{method}\n{path}\n{query}\n");
        for (name, value) in headers.iter() {
            canonical.push_str(name);
            canonical.push(':');
            canonical.push_str(value.trim());
            canonical.push('\n');
        }
        canonical.push('\n');
        canonical.push_str(&signed);
        canonical.push('\n');
        canonical.push_str(payload_hash);
        let signature = self.signature(&canonical, now);
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{},SignedHeaders={signed},Signature={signature}",
            self.access_key,
            self.scope(now)
        )
    }

    /// A GET link valid for `ttl` (capped at 7 days) from `now`.
    pub(super) fn presign(&self, key: &str, ttl: Duration, now: SystemTime) -> String {
        let (scheme, host, path) = self.locate(key);
        let ttl = ttl.min(MAX_PRESIGN).as_secs().max(1);
        let credential = format!("{}/{}", self.access_key, self.scope(now));
        let mut query = vec![
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_owned()),
            ("X-Amz-Credential", credential),
            ("X-Amz-Date", amz_date(now)),
            ("X-Amz-Expires", ttl.to_string()),
            ("X-Amz-SignedHeaders", "host".to_owned()),
        ];
        if let Some(token) = &self.session_token {
            query.push(("X-Amz-Security-Token", token.clone()));
        }
        query.sort_by(|a, b| a.0.cmp(b.0));
        let query: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{k}={}", encode_component(v)))
            .collect();
        let query = query.join("&");
        let canonical = format!("GET\n{path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
        let signature = self.signature(&canonical, now);
        format!("{scheme}://{host}{path}?{query}&X-Amz-Signature={signature}")
    }

    fn scope(&self, now: SystemTime) -> String {
        format!("{}/{}/s3/aws4_request", &amz_date(now)[..8], self.region)
    }

    fn signature(&self, canonical: &str, now: SystemTime) -> String {
        let date = amz_date(now);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{date}\n{}\n{}",
            self.scope(now),
            hex(&Sha256::digest(canonical.as_bytes()))
        );
        let key = [&date[..8], self.region.as_str(), "s3", "aws4_request"]
            .iter()
            .fold(
                format!("AWS4{}", self.secret_key).into_bytes(),
                |key, part| hmac(&key, part.as_bytes()),
            );
        hex(&hmac(&key, to_sign.as_bytes()))
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode a query value as SigV4 wants (everything but unreserved).
fn encode_component(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(s, SET).to_string()
}

fn expect_ok(res: (StatusCode, Bytes), what: &str) -> Result<()> {
    expect_ok_body(res, what).map(drop)
}

fn expect_ok_body((status, body): (StatusCode, Bytes), what: &str) -> Result<Bytes> {
    if status.is_success() {
        return Ok(body);
    }
    let text = String::from_utf8_lossy(&body);
    let code = between(&text, "<Code>", "</Code>").unwrap_or("");
    let message = between(&text, "<Message>", "</Message>").unwrap_or("");
    Err(Error::service_unavailable(format!(
        "S3 could not {what}: {status} {code} {message}"
    )))
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = s.find(start)? + start.len();
    let len = s[from..].find(end)?;
    Some(&s[from..from + len])
}

/// `YYYYMMDDTHHMMSSZ` in UTC.
fn amz_date(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn example() -> S3 {
        S3::new("examplebucket", "us-east-1").credentials(ACCESS, SECRET)
    }

    fn may_24_2013() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_369_353_600)
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(amz_date(may_24_2013()), "20130524T000000Z");
        assert_eq!(
            amz_date(UNIX_EPOCH + Duration::from_secs(951_782_400 + 3_723)),
            "20000229T010203Z"
        );
    }

    // The examples from the AWS docs ("Signature calculations for the
    // Authorization header" and "Authenticating requests: query parameters").
    #[test]
    fn header_signature_matches_the_aws_example() {
        let mut headers = vec![
            ("host", "examplebucket.s3.amazonaws.com".to_owned()),
            ("range", "bytes=0-9".to_owned()),
            (
                "x-amz-content-sha256",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
            ),
            ("x-amz-date", "20130524T000000Z".to_owned()),
        ];
        let auth = example().authorization(
            "GET",
            "/test.txt",
            "",
            &mut headers,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            may_24_2013(),
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn presigned_url_matches_the_aws_example() {
        let url = example().presign("test.txt", Duration::from_secs(86_400), may_24_2013());
        assert_eq!(
            url,
            "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    // Cross-checked with botocore's `S3SigV4QueryAuth` at the same instant.
    #[test]
    fn presigned_url_on_an_endpoint_matches_botocore() {
        let s3 = S3::new("zapline-test", "us-east-1")
            .endpoint("http://localhost:5055")
            .credentials(ACCESS, SECRET);
        let url = s3.presign(
            "s3 test/naïve file.json",
            Duration::from_secs(60),
            may_24_2013(),
        );
        assert_eq!(
            url,
            "http://localhost:5055/zapline-test/s3%20test/na%C3%AFve%20file.json\
             ?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=60&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=ffc972f0585788903e615193f7bfb8d9b68b95c1efc8a83b64349e413cecc71f"
        );
    }

    #[test]
    fn endpoints_are_path_style() {
        let s3 = S3::new("files", "auto").endpoint("http://localhost:9000/");
        let (scheme, host, path) = s3.locate("a b/ü.png");
        assert_eq!((scheme, host.as_str()), ("http", "localhost:9000"));
        assert_eq!(path, "/files/a%20b/%C3%BC.png");
        let (_, host, _) = S3::new("b", "eu-west-1").locate("k");
        assert_eq!(host, "b.s3.eu-west-1.amazonaws.com");
    }

    #[test]
    fn errors_carry_the_s3_code() {
        let body = Bytes::from_static(
            b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
        );
        let err = expect_ok((StatusCode::FORBIDDEN, body), "store the file").unwrap_err();
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            err.message().contains("AccessDenied Access Denied"),
            "{}",
            err.message()
        );
    }
}
