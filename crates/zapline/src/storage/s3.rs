//! A minimal S3 client: PUT, GET (buffered or streamed), HEAD, DELETE,
//! server-side copy, ListObjectsV2, multipart uploads, and presigned GET
//! and PUT links, signed with AWS Signature Version 4.

use std::fmt;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_core::Stream;
use hmac::{Hmac, Mac};
use http::{HeaderMap, Method, Request as HttpRequest, StatusCode};
use http_body_util::{BodyExt, Full};
use sha2::{Digest, Sha256};

use super::{encode_path, encode_query, hex, next_chunk, ObjectInfo};
use crate::body::BoxError;
use crate::{Body, Error, Result};

/// The longest a presigned link may last (an S3 limit).
const MAX_PRESIGN: Duration = Duration::from_secs(7 * 24 * 3600);

/// The size of each part of a multipart upload (S3 wants at least 5 MiB
/// for every part but the last).
const PART_SIZE: usize = 8 * 1024 * 1024;

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

/// One request to S3, before signing.
struct Call<'a> {
    method: Method,
    /// The object, or `None` for the bucket itself.
    key: Option<&'a str>,
    /// Query parameters, unencoded.
    query: Vec<(&'static str, String)>,
    /// Extra `x-amz-*` headers (signed).
    amz: Vec<(&'static str, String)>,
    /// `Content-Type` and `Cache-Control` of a stored object (unsigned).
    content_type: Option<&'a str>,
    cache_control: Option<&'a str>,
    body: Bytes,
}

impl<'a> Call<'a> {
    fn new(method: Method, key: Option<&'a str>) -> Self {
        Call {
            method,
            key,
            query: Vec::new(),
            amz: Vec::new(),
            content_type: None,
            cache_control: None,
            body: Bytes::new(),
        }
    }
}

/// A buffered S3 response.
#[derive(Debug)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl S3 {
    /// A bucket on AWS in `region` (e.g. `us-east-1`). Set the credentials
    /// with [`credentials`](Self::credentials).
    ///
    /// ```
    /// let s3 = zapline::storage::S3::new("my-bucket", "eu-west-1");
    /// ```
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        S3 {
            bucket: bucket.into(),
            region: region.into(),
            endpoint: None,
            access_key: String::new(),
            secret_key: String::new(),
            session_token: None,
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

    /// `(scheme, host, path)` of an object, or of the bucket for `None`.
    fn locate(&self, key: Option<&str>) -> (&str, String, String) {
        let key = key.map(encode_path);
        match &self.endpoint {
            Some(endpoint) => {
                let (scheme, host) = endpoint.split_once("://").unwrap_or(("https", endpoint));
                let bucket = encode_path(&self.bucket);
                let path = match key {
                    Some(key) => format!("/{bucket}/{key}"),
                    None => format!("/{bucket}"),
                };
                (scheme, host.to_owned(), path)
            }
            None => {
                let host = if self.region == "us-east-1" {
                    format!("{}.s3.amazonaws.com", self.bucket)
                } else {
                    format!("{}.s3.{}.amazonaws.com", self.bucket, self.region)
                };
                ("https", host, format!("/{}", key.unwrap_or_default()))
            }
        }
    }

    pub(super) async fn put(
        &self,
        key: &str,
        body: Bytes,
        (content_type, cache_control): &(String, Option<String>),
    ) -> Result<()> {
        let mut call = Call::new(Method::PUT, Some(key));
        call.body = body;
        call.content_type = Some(content_type);
        call.cache_control = cache_control.as_deref();
        expect_ok(self.send(call).await?, "store the file").map(drop)
    }

    /// Store a stream: one `PUT` when it fits in a part, a multipart
    /// upload otherwise (aborted if anything fails).
    pub(super) async fn put_stream<S, B, E>(
        &self,
        key: &str,
        (content_type, cache_control): &(String, Option<String>),
        mut stream: Pin<&mut S>,
    ) -> Result<u64>
    where
        S: Stream<Item = std::result::Result<B, E>> + Send,
        B: Into<Bytes>,
        E: Into<BoxError>,
    {
        let (first, mut done) = fill_part(&mut stream).await?;
        if done {
            let size = first.len() as u64;
            let options = (content_type.clone(), cache_control.clone());
            self.put(key, first.into(), &options).await?;
            return Ok(size);
        }
        let mut call = Call::new(Method::POST, Some(key));
        call.query.push(("uploads", String::new()));
        call.content_type = Some(content_type);
        call.cache_control = cache_control.as_deref();
        let created = expect_ok(self.send(call).await?, "start the upload")?;
        let text = String::from_utf8_lossy(&created.body);
        let upload_id = between(&text, "<UploadId>", "</UploadId>")
            .map(xml_unescape)
            .ok_or_else(|| Error::service_unavailable("S3 did not return an upload id"))?;

        let uploaded = async {
            let mut etags = Vec::new();
            let mut size = 0u64;
            let mut part = first;
            loop {
                size += part.len() as u64;
                let mut call = Call::new(Method::PUT, Some(key));
                call.query
                    .push(("partNumber", (etags.len() + 1).to_string()));
                call.query.push(("uploadId", upload_id.clone()));
                call.body = part.into();
                let reply = expect_ok(self.send(call).await?, "store part of the file")?;
                let etag = reply
                    .headers
                    .get(http::header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| Error::service_unavailable("S3 did not return a part ETag"))?;
                etags.push(etag.to_owned());
                if done {
                    break;
                }
                (part, done) = fill_part(&mut stream).await?;
                if part.is_empty() {
                    break;
                }
            }
            let mut xml = String::from("<CompleteMultipartUpload>");
            for (i, etag) in etags.iter().enumerate() {
                xml.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                    i + 1,
                    xml_escape(etag)
                ));
            }
            xml.push_str("</CompleteMultipartUpload>");
            let mut call = Call::new(Method::POST, Some(key));
            call.query.push(("uploadId", upload_id.clone()));
            call.body = xml.into();
            expect_ok(self.send(call).await?, "finish the upload")?;
            Ok::<_, Error>(size)
        }
        .await;
        if uploaded.is_err() {
            let mut call = Call::new(Method::DELETE, Some(key));
            call.query.push(("uploadId", upload_id));
            // Best effort: a lifecycle rule cleans up what this misses.
            let _ = self.send(call).await;
        }
        uploaded
    }

    pub(super) async fn get(&self, key: &str) -> Result<Bytes> {
        let reply = self.send(Call::new(Method::GET, Some(key))).await?;
        match reply.status {
            StatusCode::NOT_FOUND => Err(Error::not_found()),
            _ => expect_ok(reply, "read the file").map(|r| r.body),
        }
    }

    pub(super) async fn get_stream(&self, key: &str) -> Result<Body> {
        let req = self.request(Call::new(Method::GET, Some(key)), SystemTime::now())?;
        let res = crate::http_client::fetch_streaming(req, "S3").await?;
        if res.status().is_success() {
            return Ok(Body::stream(res.into_body()));
        }
        let (parts, body) = res.into_parts();
        if parts.status == StatusCode::NOT_FOUND {
            return Err(Error::not_found());
        }
        let body = body
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        let reply = Reply {
            status: parts.status,
            headers: parts.headers,
            body,
        };
        expect_ok(reply, "read the file").map(|_| Body::empty())
    }

    pub(super) async fn exists(&self, key: &str) -> Result<bool> {
        let reply = self.send(Call::new(Method::HEAD, Some(key))).await?;
        match reply.status {
            StatusCode::NOT_FOUND => Ok(false),
            _ => expect_ok(reply, "check the file").map(|_| true),
        }
    }

    pub(super) async fn metadata(&self, key: &str) -> Result<ObjectInfo> {
        let reply = self.send(Call::new(Method::HEAD, Some(key))).await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Err(Error::not_found());
        }
        let reply = expect_ok(reply, "read the file's metadata")?;
        let header = |name: http::HeaderName| {
            reply
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        Ok(ObjectInfo {
            key: key.to_owned(),
            size: header(http::header::CONTENT_LENGTH)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            modified: header(http::header::LAST_MODIFIED)
                .and_then(|v| httpdate::parse_http_date(&v).ok())
                .unwrap_or(UNIX_EPOCH),
            content_type: header(http::header::CONTENT_TYPE),
            cache_control: header(http::header::CACHE_CONTROL),
        })
    }

    /// Every object under `prefix`, following continuation tokens.
    pub(super) async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let reply = self.send(self.list_call(prefix, token.as_deref())).await?;
            let reply = expect_ok(reply, "list the files")?;
            let page = parse_list(&String::from_utf8_lossy(&reply.body));
            out.extend(page.objects);
            match page.next {
                Some(next) if page.truncated => token = Some(next),
                _ => return Ok(out),
            }
        }
    }

    fn list_call<'a>(&self, prefix: &str, token: Option<&str>) -> Call<'a> {
        let mut call = Call::new(Method::GET, None);
        call.query.push(("list-type", "2".to_owned()));
        call.query.push(("encoding-type", "url".to_owned()));
        if !prefix.is_empty() {
            call.query.push(("prefix", prefix.to_owned()));
        }
        if let Some(token) = token {
            call.query.push(("continuation-token", token.to_owned()));
        }
        call
    }

    /// Copy inside the bucket (`CopyObject`), keeping the content type.
    pub(super) async fn copy(&self, from: &str, to: &str) -> Result<()> {
        let mut call = Call::new(Method::PUT, Some(to));
        call.amz.push((
            "x-amz-copy-source",
            format!("/{}/{}", encode_path(&self.bucket), encode_path(from)),
        ));
        let reply = self.send(call).await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Err(Error::not_found());
        }
        let reply = expect_ok(reply, "copy the file")?;
        // A copy can fail after S3 has answered 200: the error is the body.
        if String::from_utf8_lossy(&reply.body).contains("<Error>") {
            return expect_ok(
                Reply {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    ..reply
                },
                "copy the file",
            )
            .map(drop);
        }
        Ok(())
    }

    pub(super) async fn delete(&self, key: &str) -> Result<()> {
        let reply = self.send(Call::new(Method::DELETE, Some(key))).await?;
        expect_ok(reply, "delete the file").map(drop)
    }

    async fn send(&self, call: Call<'_>) -> Result<Reply> {
        let req = self.request(call, SystemTime::now())?;
        let res = crate::http_client::fetch(req, "S3").await?;
        Ok(Reply {
            status: res.status,
            headers: res.headers,
            body: res.body,
        })
    }

    /// The signed HTTP request for `call` at `now`.
    fn request(&self, call: Call<'_>, now: SystemTime) -> Result<HttpRequest<Full<Bytes>>> {
        let (scheme, host, path) = self.locate(call.key);
        let query = canonical_query(&call.query);
        let payload_hash = hex(&Sha256::digest(&call.body));
        let mut headers = vec![
            ("host", host.clone()),
            ("x-amz-content-sha256", payload_hash.clone()),
            ("x-amz-date", amz_date(now)),
        ];
        headers.extend(call.amz.iter().cloned());
        if let Some(token) = &self.session_token {
            headers.push(("x-amz-security-token", token.clone()));
        }
        let method = call.method;
        let auth = self.authorization(
            method.as_str(),
            &path,
            &query,
            &mut headers,
            &payload_hash,
            now,
        );

        let uri = if query.is_empty() {
            format!("{scheme}://{host}{path}")
        } else {
            format!("{scheme}://{host}{path}?{query}")
        };
        let mut req = HttpRequest::builder().method(method).uri(uri);
        for (name, value) in &headers {
            if *name != "host" {
                req = req.header(*name, value);
            }
        }
        if let Some(content_type) = call.content_type {
            req = req.header(http::header::CONTENT_TYPE, content_type);
        }
        if let Some(cache_control) = call.cache_control {
            req = req.header(http::header::CACHE_CONTROL, cache_control);
        }
        req.header(http::header::AUTHORIZATION, auth)
            .body(Full::new(call.body))
            .map_err(|e| Error::internal("invalid S3 request").with_source(e))
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
        self.presign_with("GET", key, ttl, &[], now).0
    }

    /// A PUT link valid for `ttl` (capped at 7 days) from `now`, for an
    /// upload sent with `Content-Type: content_type`. Returns the link and
    /// how many seconds it lasts.
    pub(super) fn presign_put(
        &self,
        key: &str,
        ttl: Duration,
        content_type: &str,
        now: SystemTime,
    ) -> (String, u64) {
        self.presign_with("PUT", key, ttl, &[("content-type", content_type)], now)
    }

    /// A presigned `method` link that also signs `headers` (lowercase
    /// names), and its lifetime in seconds.
    fn presign_with(
        &self,
        method: &str,
        key: &str,
        ttl: Duration,
        headers: &[(&str, &str)],
        now: SystemTime,
    ) -> (String, u64) {
        let (scheme, host, path) = self.locate(Some(key));
        let ttl = ttl.min(MAX_PRESIGN).as_secs().max(1);
        let mut signed: Vec<(&str, &str)> = headers.to_vec();
        signed.push(("host", &host));
        signed.sort_by(|a, b| a.0.cmp(b.0));
        let names: Vec<&str> = signed.iter().map(|(n, _)| *n).collect();
        let names = names.join(";");
        let credential = format!("{}/{}", self.access_key, self.scope(now));
        let mut query = vec![
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_owned()),
            ("X-Amz-Credential", credential),
            ("X-Amz-Date", amz_date(now)),
            ("X-Amz-Expires", ttl.to_string()),
            ("X-Amz-SignedHeaders", names.clone()),
        ];
        if let Some(token) = &self.session_token {
            query.push(("X-Amz-Security-Token", token.clone()));
        }
        let query = canonical_query(&query);
        let mut canonical = format!("{method}\n{path}\n{query}\n");
        for (name, value) in &signed {
            canonical.push_str(&format!("{name}:{}\n", value.trim()));
        }
        canonical.push_str(&format!("\n{names}\nUNSIGNED-PAYLOAD"));
        let signature = self.signature(&canonical, now);
        (
            format!("{scheme}://{host}{path}?{query}&X-Amz-Signature={signature}"),
            ttl,
        )
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

/// Read up to a part's worth of `stream`: the bytes, and whether the stream
/// has ended.
async fn fill_part<S, B, E>(stream: &mut Pin<&mut S>) -> Result<(Vec<u8>, bool)>
where
    S: Stream<Item = std::result::Result<B, E>>,
    B: Into<Bytes>,
    E: Into<BoxError>,
{
    let mut part = Vec::new();
    while part.len() < PART_SIZE {
        match next_chunk(stream).await? {
            Some(chunk) => part.extend_from_slice(&chunk),
            None => return Ok((part, true)),
        }
    }
    Ok((part, false))
}

/// The query string as SigV4 signs it: names and values percent-encoded,
/// sorted by name.
fn canonical_query(params: &[(&str, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (encode_query(k), encode_query(v)))
        .collect();
    encoded.sort();
    let pairs: Vec<String> = encoded.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.join("&")
}

/// One page of a ListObjectsV2 response.
struct ListPage {
    objects: Vec<ObjectInfo>,
    truncated: bool,
    next: Option<String>,
}

/// Parse a ListObjectsV2 response made with `encoding-type=url`. Keys that
/// aren't valid storage keys are skipped.
fn parse_list(xml: &str) -> ListPage {
    let mut objects = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let after = &rest[start + "<Contents>".len()..];
        let end = after.find("</Contents>").unwrap_or(after.len());
        let entry = &after[..end];
        rest = &after[end..];
        let Some(key) = between(entry, "<Key>", "</Key>") else {
            continue;
        };
        let key = percent_encoding::percent_decode_str(&xml_unescape(key))
            .decode_utf8_lossy()
            .into_owned();
        if super::check_key(&key).is_err() {
            continue;
        }
        objects.push(ObjectInfo {
            key,
            size: between(entry, "<Size>", "</Size>")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0),
            modified: between(entry, "<LastModified>", "</LastModified>")
                .and_then(parse_iso8601)
                .unwrap_or(UNIX_EPOCH),
            content_type: None,
            cache_control: None,
        });
    }
    ListPage {
        objects,
        truncated: between(xml, "<IsTruncated>", "</IsTruncated>")
            .is_some_and(|v| v.trim() == "true"),
        next: between(xml, "<NextContinuationToken>", "</NextContinuationToken>").map(xml_unescape),
    }
}

/// `2009-10-12T17:50:30.000Z` (what S3 lists) as a time.
fn parse_iso8601(s: &str) -> Option<SystemTime> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut date = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.split('.').next()?;
    let mut time = time.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, sec) = (time.next()??, time.next()??, time.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = u64::try_from(era * 146_097 + doe - 719_468).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(days * 86_400 + h * 3600 + m * 60 + sec))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn expect_ok(reply: Reply, what: &str) -> Result<Reply> {
    if reply.status.is_success() {
        return Ok(reply);
    }
    let text = String::from_utf8_lossy(&reply.body);
    let code = between(&text, "<Code>", "</Code>").unwrap_or("");
    let message = between(&text, "<Message>", "</Message>").unwrap_or("");
    Err(Error::service_unavailable(format!(
        "S3 could not {what}: {} {code} {message}",
        reply.status
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
        let (scheme, host, path) = s3.locate(Some("a b/ü.png"));
        assert_eq!((scheme, host.as_str()), ("http", "localhost:9000"));
        assert_eq!(path, "/files/a%20b/%C3%BC.png");
        assert_eq!(s3.locate(None).2, "/files");
        let (_, host, path) = S3::new("b", "eu-west-1").locate(Some("k"));
        assert_eq!(
            (host.as_str(), path.as_str()),
            ("b.s3.eu-west-1.amazonaws.com", "/k")
        );
        assert_eq!(S3::new("b", "eu-west-1").locate(None).2, "/");
    }

    fn reply(status: StatusCode, body: &'static [u8]) -> Reply {
        Reply {
            status,
            headers: HeaderMap::new(),
            body: Bytes::from_static(body),
        }
    }

    #[test]
    fn errors_carry_the_s3_code() {
        let body = b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
        let err = expect_ok(reply(StatusCode::FORBIDDEN, body), "store the file").unwrap_err();
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            err.message().contains("AccessDenied Access Denied"),
            "{}",
            err.message()
        );
    }

    fn authorization_of(req: &HttpRequest<Full<Bytes>>) -> &str {
        req.headers()["authorization"].to_str().unwrap()
    }

    // "GET Bucket (List Objects)" from the AWS Signature V4 examples.
    #[test]
    fn bucket_listing_signature_matches_the_aws_example() {
        let mut call = Call::new(Method::GET, None);
        call.query.push(("prefix", "J".to_owned()));
        call.query.push(("max-keys", "2".to_owned()));
        let req = example().request(call, may_24_2013()).unwrap();
        assert_eq!(
            req.uri().to_string(),
            "https://examplebucket.s3.amazonaws.com/?max-keys=2&prefix=J"
        );
        assert_eq!(
            authorization_of(&req),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;x-amz-content-sha256;x-amz-date,\
             Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    // Cross-checked with botocore's `S3SigV4Auth` at the same instant.
    #[test]
    fn list_objects_v2_signature_matches_botocore() {
        let s3 = S3::new("zapline-test", "us-east-1")
            .endpoint("http://localhost:9000")
            .credentials(ACCESS, SECRET);
        let call = s3.list_call("a b/", Some("1/abc=="));
        let req = s3.request(call, may_24_2013()).unwrap();
        assert_eq!(
            req.uri().to_string(),
            "http://localhost:9000/zapline-test?continuation-token=1%2Fabc%3D%3D\
             &encoding-type=url&list-type=2&prefix=a%20b%2F"
        );
        assert_eq!(
            authorization_of(&req),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;x-amz-content-sha256;x-amz-date,\
             Signature=b4378018f41e532e64a9b1380ff2cf714a22836dc16b7267f4165acf08f98b42"
        );
    }

    // Cross-checked with botocore's `S3SigV4QueryAuth` at the same instant.
    #[test]
    fn presigned_put_signs_the_content_type_like_botocore() {
        let (url, ttl) = example().presign_put(
            "uploads/my photo.png",
            Duration::from_secs(600),
            "image/png",
            may_24_2013(),
        );
        assert_eq!(ttl, 600);
        assert_eq!(
            url,
            "https://examplebucket.s3.amazonaws.com/uploads/my%20photo.png\
             ?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=600&X-Amz-SignedHeaders=content-type%3Bhost\
             &X-Amz-Signature=9038946bde9ff0ed8a77c07bc23be749cad1eb518019dd024dbce1c9bd3bc02b"
        );
        let month = Duration::from_secs(30 * 86_400);
        let (_, ttl) = example().presign_put("k", month, "a/b", may_24_2013());
        assert_eq!(ttl, 7 * 86_400, "capped at a week");
    }

    #[test]
    fn copies_sign_the_source_header() {
        let mut call = Call::new(Method::PUT, Some("b/2.txt"));
        call.amz
            .push(("x-amz-copy-source", "/examplebucket/a/1%20x.txt".to_owned()));
        let req = example().request(call, may_24_2013()).unwrap();
        assert_eq!(
            req.headers()["x-amz-copy-source"],
            "/examplebucket/a/1%20x.txt"
        );
        assert!(authorization_of(&req)
            .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-copy-source;x-amz-date,"));
    }

    #[test]
    fn listings_are_parsed_with_their_continuation_token() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult><Name>b</Name>\
            <Prefix>a%20b/</Prefix><KeyCount>3</KeyCount><IsTruncated>true</IsTruncated>\
            <NextContinuationToken>1/abc&amp;==</NextContinuationToken>\
            <Contents><Key>a%20b/1.png</Key><LastModified>2013-05-24T01:02:03.000Z</LastModified>\
            <ETag>&quot;x&quot;</ETag><Size>434234</Size><StorageClass>STANDARD</StorageClass></Contents>\
            <Contents><Key>a%20b/na%C3%AFve%26.txt</Key>\
            <LastModified>2000-02-29T00:00:00Z</LastModified><Size>0</Size></Contents>\
            <Contents><Key>a%20b/../evil</Key><Size>1</Size></Contents>\
            </ListBucketResult>";
        let page = parse_list(xml);
        assert!(page.truncated);
        assert_eq!(page.next.as_deref(), Some("1/abc&=="));
        let keys: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            ["a b/1.png", "a b/naïve&.txt"],
            "invalid keys skipped"
        );
        assert_eq!(page.objects[0].size, 434_234);
        assert_eq!(
            page.objects[0].modified,
            may_24_2013() + Duration::from_secs(3723)
        );
        assert_eq!(
            page.objects[1].modified,
            UNIX_EPOCH + Duration::from_secs(951_782_400)
        );
        let last =
            parse_list("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>");
        assert!(!last.truncated && last.objects.is_empty() && last.next.is_none());
    }

    #[test]
    fn iso_dates_round_trip_with_amz_dates() {
        for secs in [0, 951_782_400 + 3_723, 1_369_353_600, 4_102_444_799] {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            let amz = amz_date(t);
            let iso = format!(
                "{}-{}-{}T{}:{}:{}.000Z",
                &amz[0..4],
                &amz[4..6],
                &amz[6..8],
                &amz[9..11],
                &amz[11..13],
                &amz[13..15]
            );
            assert_eq!(parse_iso8601(&iso), Some(t), "{iso}");
        }
        assert_eq!(parse_iso8601("2013-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601("garbage"), None);
    }
}
