//! Response compression: brotli, zstd, gzip and deflate, negotiated from
//! `Accept-Encoding`.
//!
//! ```
//! use zapline::prelude::*;
//! use zapline::middleware::Compression;
//!
//! let app = App::new()
//!     .wrap(Compression::new())
//!     .get("/", || async { "hello ".repeat(1000) });
//! ```

use std::borrow::Cow;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
use async_compression::Level;
use bytes::{Bytes, BytesMut};
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, StatusCode};
use http_body::{Body as _, Frame};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt};
use tokio_util::io::{poll_read_buf, StreamReader};

use super::{Middleware, Next};
use crate::body::Body;
use crate::handler::BoxFuture;
use crate::request::Request;
use crate::response::Response;

/// Size of each compressed chunk read from a streaming encoder.
const CHUNK: usize = 8 * 1024;

/// Buffered bodies up to this size are compressed in one go (and keep a
/// `Content-Length`); larger ones are streamed so other requests on the
/// same core get a turn in between chunks.
const EAGER_MAX: usize = 1024 * 1024;

/// Compression effort.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressionLevel {
    /// Fastest, larger output.
    Fastest,
    /// A balanced level per algorithm (brotli 4, zstd 3, gzip/deflate 6).
    #[default]
    Default,
    /// Smallest output, slowest; usually too slow for dynamic responses.
    Best,
    /// An algorithm-specific level, clamped to the algorithm's range.
    Precise(i32),
}

impl CompressionLevel {
    fn for_encoding(self, enc: Encoding) -> Level {
        match (self, enc) {
            // brotli's own default (11) is far too slow for on-the-fly use.
            (CompressionLevel::Default, Encoding::Br) => Level::Precise(4),
            (CompressionLevel::Default, _) => Level::Default,
            (CompressionLevel::Fastest, _) => Level::Fastest,
            (CompressionLevel::Best, _) => Level::Best,
            (CompressionLevel::Precise(q), _) => Level::Precise(q),
        }
    }
}

/// Supported encodings, in tie-break preference order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Br,
    Zstd,
    Gzip,
    Deflate,
}

const ENCODINGS: [Encoding; 4] = [
    Encoding::Br,
    Encoding::Zstd,
    Encoding::Gzip,
    Encoding::Deflate,
];

impl Encoding {
    fn header(self) -> HeaderValue {
        HeaderValue::from_static(match self {
            Encoding::Br => "br",
            Encoding::Zstd => "zstd",
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
        })
    }

    fn from_token(s: &str) -> Option<Self> {
        [
            ("br", Encoding::Br),
            ("zstd", Encoding::Zstd),
            ("gzip", Encoding::Gzip),
            ("x-gzip", Encoding::Gzip),
            ("deflate", Encoding::Deflate),
        ]
        .into_iter()
        .find(|(name, _)| s.eq_ignore_ascii_case(name))
        .map(|(_, e)| e)
    }
}

type Predicate = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Compresses response bodies with the best encoding the client accepts.
///
/// - Negotiates from `Accept-Encoding` q-values (`q=0` excludes, `*`
///   matches unlisted encodings); ties prefer br > zstd > gzip > deflate.
/// - Skips `HEAD`, 1xx/204/206/304 responses, responses that already have
///   a `Content-Encoding`, `Cache-Control: no-transform`, bodies known to
///   be smaller than [`min_size`](Self::min_size) and content types that
///   don't compress (see [`is_compressible`]).
/// - Sets `Content-Encoding`, adds `Vary: accept-encoding`, drops
///   `Content-Length` and `Accept-Ranges`, and weakens a strong `ETag`.
/// - Buffered bodies up to 1 MiB are compressed in one go (keeping a
///   `Content-Length`); larger and streaming bodies are compressed as they
///   stream, flushing whenever the source pauses so streams (e.g. SSE, if
///   you enable it) still arrive promptly.
///
/// ```
/// use zapline::middleware::{Compression, CompressionLevel};
/// use zapline::middleware::compression::is_compressible;
///
/// let mw = Compression::new()
///     .zstd(false)
///     .quality(CompressionLevel::Fastest)
///     .min_size(1024)
///     .content_types(|ct| is_compressible(ct) || ct == "text/event-stream");
/// ```
#[derive(Clone)]
pub struct Compression {
    enabled: [bool; 4],
    level: CompressionLevel,
    min_size: u64,
    predicate: Option<Predicate>,
}

impl Default for Compression {
    fn default() -> Self {
        Self::new()
    }
}

impl Compression {
    /// All encodings enabled, default level, 860-byte minimum.
    pub fn new() -> Self {
        Compression {
            enabled: [true; 4],
            level: CompressionLevel::Default,
            min_size: 860,
            predicate: None,
        }
    }

    fn set(mut self, enc: Encoding, on: bool) -> Self {
        self.enabled[enc as usize] = on;
        self
    }

    /// Enable or disable gzip.
    pub fn gzip(self, on: bool) -> Self {
        self.set(Encoding::Gzip, on)
    }

    /// Enable or disable brotli (`br`).
    pub fn br(self, on: bool) -> Self {
        self.set(Encoding::Br, on)
    }

    /// Enable or disable zstd.
    pub fn zstd(self, on: bool) -> Self {
        self.set(Encoding::Zstd, on)
    }

    /// Enable or disable deflate (zlib-wrapped, as HTTP specifies).
    pub fn deflate(self, on: bool) -> Self {
        self.set(Encoding::Deflate, on)
    }

    /// Compression level for every encoding.
    pub fn quality(mut self, level: CompressionLevel) -> Self {
        self.level = level;
        self
    }

    /// Don't compress bodies known to be smaller than `bytes` (default 860).
    pub fn min_size(mut self, bytes: u64) -> Self {
        self.min_size = bytes;
        self
    }

    /// Decide which content types to compress. The predicate gets the
    /// lowercase media type without parameters (e.g. `application/json`).
    /// Default: [`is_compressible`].
    pub fn content_types<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.predicate = Some(Arc::new(predicate));
        self
    }

    /// Whether this response should be compressed (ignoring the client).
    fn eligible(&self, res: &Response) -> bool {
        let status = res.status();
        if status.is_informational()
            || matches!(
                status,
                StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED | StatusCode::PARTIAL_CONTENT
            )
        {
            return false;
        }
        let h = res.headers();
        if h.contains_key(header::CONTENT_ENCODING)
            || h.contains_key(header::CONTENT_RANGE)
            || has_token(h, header::CACHE_CONTROL, "no-transform")
        {
            return false;
        }
        if let Some(n) = res.body().size_hint().exact() {
            if n == 0 || n < self.min_size {
                return false;
            }
        }
        let Some(ct) = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        let essence = ct.split(';').next().unwrap_or("").trim();
        let essence = if essence.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(essence.to_ascii_lowercase())
        } else {
            Cow::Borrowed(essence)
        };
        match &self.predicate {
            Some(p) => p(&essence),
            None => is_compressible(&essence),
        }
    }

    async fn compress(&self, res: Response, enc: Encoding) -> Response {
        let level = self.level.for_encoding(enc);
        let (mut parts, body) = res.into_parts();
        let body = match body {
            Body::Full(Some(bytes)) if bytes.len() <= EAGER_MAX => {
                let mut out = Vec::with_capacity(bytes.len() / 3 + 64);
                let mut reader = encoder(enc, io::Cursor::new(bytes.clone()), level);
                if reader.read_to_end(&mut out).await.is_err() {
                    // Can't happen for in-memory input; send it uncompressed.
                    return Response::from_parts(parts, Body::Full(Some(bytes)));
                }
                Body::from(out)
            }
            body => {
                let reader = StreamReader::new(DataStream(body));
                Body::stream(Compressed {
                    reader: encoder(enc, reader, level),
                    buf: BytesMut::new(),
                    done: false,
                })
            }
        };
        let h = &mut parts.headers;
        h.insert(header::CONTENT_ENCODING, enc.header());
        h.remove(header::CONTENT_LENGTH);
        h.remove(header::ACCEPT_RANGES);
        if let Some(etag) = h.get(header::ETAG) {
            if etag.as_bytes().starts_with(b"\"") {
                let mut weak = b"W/".to_vec();
                weak.extend_from_slice(etag.as_bytes());
                if let Ok(v) = HeaderValue::from_bytes(&weak) {
                    h.insert(header::ETAG, v);
                }
            }
        }
        Response::from_parts(parts, body)
    }
}

impl Middleware for Compression {
    fn handle(&self, req: Request, next: Next) -> BoxFuture<Response> {
        if req.method() == Method::HEAD {
            return next.run(req);
        }
        let encoding = negotiate(req.headers(), &self.enabled);
        let this = self.clone();
        let fut = next.run(req);
        Box::pin(async move {
            let mut res = fut.await;
            if !this.eligible(&res) {
                return res;
            }
            add_vary(res.headers_mut());
            match encoding {
                Some(enc) => this.compress(res, enc).await,
                None => res,
            }
        })
    }
}

/// The default compressible-type check: text (except `text/event-stream`),
/// JSON, JavaScript, XML, SVG, WebAssembly, YAML, TOML, form data,
/// uncompressed fonts (`ttf`, `otf`) and any `+json` / `+xml` type.
/// Images, audio, video, archives and `woff`/`woff2` are already compressed.
///
/// Expects a lowercase media type without parameters.
pub fn is_compressible(content_type: &str) -> bool {
    let Some((ty, sub)) = content_type.split_once('/') else {
        return false;
    };
    match ty {
        "text" => sub != "event-stream",
        "application" => {
            sub.ends_with("+json")
                || sub.ends_with("+xml")
                || matches!(
                    sub,
                    "json"
                        | "javascript"
                        | "x-javascript"
                        | "ecmascript"
                        | "xml"
                        | "wasm"
                        | "x-ndjson"
                        | "ndjson"
                        | "jsonl"
                        | "x-www-form-urlencoded"
                        | "graphql"
                        | "yaml"
                        | "x-yaml"
                        | "toml"
                        | "rtf"
                        | "x-sh"
                        | "x-tar"
                        | "sql"
                        | "vnd.ms-fontobject"
                        | "x-font-ttf"
                        | "x-font-otf"
                )
        }
        "image" => sub == "svg+xml",
        "font" => matches!(sub, "ttf" | "otf" | "collection" | "sfnt"),
        _ => false,
    }
}

/// Pick the encoding from `Accept-Encoding`.
fn negotiate(headers: &HeaderMap, enabled: &[bool; 4]) -> Option<Encoding> {
    // q-values in thousandths; `None` = not mentioned.
    let mut q: [Option<u16>; 4] = [None; 4];
    let mut star: Option<u16> = None;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for item in value.split(',') {
            let mut parts = item.split(';');
            let name = parts.next().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            let weight = parse_q(parts);
            let slot = if name == "*" {
                &mut star
            } else if let Some(enc) = Encoding::from_token(name) {
                &mut q[enc as usize]
            } else {
                continue;
            };
            *slot = Some(slot.map_or(weight, |w| w.max(weight)));
        }
    }
    let mut best: Option<(Encoding, u16)> = None;
    for enc in ENCODINGS {
        if !enabled[enc as usize] {
            continue;
        }
        let weight = q[enc as usize].or(star).unwrap_or(0);
        // Strictly greater: earlier (preferred) encodings win ties.
        if weight > 0 && best.is_none_or(|(_, w)| weight > w) {
            best = Some((enc, weight));
        }
    }
    best.map(|(e, _)| e)
}

/// The `q` parameter in thousandths (default 1000; invalid counts as 0).
fn parse_q<'a>(params: impl Iterator<Item = &'a str>) -> u16 {
    for p in params {
        let Some((k, v)) = p.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("q") {
            return match v.trim().parse::<f32>() {
                Ok(q) if (0.0..=1.0).contains(&q) => (q * 1000.0).round() as u16,
                _ => 0,
            };
        }
    }
    1000
}

/// Whether a comma-separated header contains `token` (case-insensitive).
fn has_token(h: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    h.get_all(name).iter().any(|v| {
        v.to_str().is_ok_and(|s| {
            s.split(',').any(|t| {
                t.split('=')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case(token)
            })
        })
    })
}

fn add_vary(h: &mut HeaderMap) {
    if !has_token(h, header::VARY, "accept-encoding") && !has_token(h, header::VARY, "*") {
        h.append(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
}

fn encoder<R>(enc: Encoding, r: R, level: Level) -> Pin<Box<dyn AsyncRead + Send>>
where
    R: AsyncBufRead + Send + 'static,
{
    match enc {
        Encoding::Br => Box::pin(BrotliEncoder::with_quality(r, level)),
        Encoding::Zstd => Box::pin(ZstdEncoder::with_quality(r, level)),
        Encoding::Gzip => Box::pin(GzipEncoder::with_quality(r, level)),
        Encoding::Deflate => Box::pin(ZlibEncoder::with_quality(r, level)),
    }
}

/// A response body as a stream of data chunks (trailers are dropped).
struct DataStream(Body);

impl futures_core::Stream for DataStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match ready!(Pin::new(&mut self.0).poll_frame(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) if data.is_empty() => continue,
                    Ok(data) => Poll::Ready(Some(Ok(data))),
                    Err(_trailers) => continue,
                },
                Some(Err(e)) => Poll::Ready(Some(Err(io::Error::other(e)))),
                None => Poll::Ready(None),
            };
        }
    }
}

/// A streaming compressed body.
struct Compressed {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    buf: BytesMut,
    done: bool,
}

impl http_body::Body for Compressed {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        if this.buf.capacity() < CHUNK {
            this.buf.reserve(CHUNK);
        }
        match ready!(poll_read_buf(this.reader.as_mut(), cx, &mut this.buf)) {
            Ok(0) => {
                this.done = true;
                Poll::Ready(None)
            }
            Ok(_) => Poll::Ready(Some(Ok(Frame::data(this.buf.split().freeze())))),
            Err(e) => {
                this.done = true;
                Poll::Ready(Some(Err(e)))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(accept: &str) -> Option<Encoding> {
        pick_with(accept, [true; 4])
    }

    fn pick_with(accept: &str, enabled: [bool; 4]) -> Option<Encoding> {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_str(accept).unwrap(),
        );
        negotiate(&h, &enabled)
    }

    #[test]
    fn negotiation() {
        assert_eq!(pick("gzip"), Some(Encoding::Gzip));
        assert_eq!(pick("gzip, deflate, br, zstd"), Some(Encoding::Br));
        assert_eq!(pick("gzip, deflate"), Some(Encoding::Gzip));
        assert_eq!(pick("deflate, gzip;q=0.9"), Some(Encoding::Deflate));
        assert_eq!(pick("br;q=0.5, gzip;q=0.8"), Some(Encoding::Gzip));
        assert_eq!(pick("br;q=0, gzip;q=0"), None);
        assert_eq!(pick("identity"), None);
        assert_eq!(pick("*"), Some(Encoding::Br));
        assert_eq!(pick("*;q=0.5, gzip"), Some(Encoding::Gzip));
        assert_eq!(pick("br;q=0, *"), Some(Encoding::Zstd));
        assert_eq!(pick("GZIP ; Q=1.0"), Some(Encoding::Gzip));
        assert_eq!(pick("x-gzip"), Some(Encoding::Gzip));
        assert_eq!(pick("gzip;q=abc, deflate;q=0.1"), Some(Encoding::Deflate));
        assert_eq!(pick("gzip;q=2"), None);
        assert_eq!(pick(""), None);
        assert_eq!(pick(" , ;q=1"), None);
        assert_eq!(
            pick_with("br, gzip", [false, true, true, true]),
            Some(Encoding::Gzip)
        );
    }

    #[test]
    fn compressible_types() {
        for ct in [
            "text/html",
            "text/plain",
            "application/json",
            "application/problem+json",
            "application/javascript",
            "image/svg+xml",
            "application/wasm",
            "application/atom+xml",
            "font/ttf",
        ] {
            assert!(is_compressible(ct), "{ct}");
        }
        for ct in [
            "text/event-stream",
            "image/png",
            "image/jpeg",
            "video/mp4",
            "audio/mpeg",
            "font/woff2",
            "application/zip",
            "application/gzip",
            "application/octet-stream",
            "garbage",
        ] {
            assert!(!is_compressible(ct), "{ct}");
        }
    }
}
