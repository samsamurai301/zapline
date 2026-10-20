//! Response compression: negotiation, skip rules, buffered and streaming
//! bodies (decoded with async-compression's decoders).
#![cfg(feature = "compression")]

use std::convert::Infallible;
use std::time::Duration;

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapline::header;
use zapline::middleware::compression::is_compressible;
use zapline::middleware::{Compression, CompressionLevel};
use zapline::prelude::*;
use zapline::test::TestClient;
use zapline::Body;

fn big_text() -> String {
    (0..400)
        .map(|i| format!("line {i} of some text\n"))
        .collect()
}

async fn decode(encoding: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match encoding {
        "gzip" => GzipDecoder::new(data).read_to_end(&mut out).await,
        "br" => BrotliDecoder::new(data).read_to_end(&mut out).await,
        "zstd" => ZstdDecoder::new(data).read_to_end(&mut out).await,
        "deflate" => ZlibDecoder::new(data).read_to_end(&mut out).await,
        other => panic!("unexpected encoding {other}"),
    }
    .unwrap_or_else(|e| panic!("invalid {encoding} data: {e}"));
    out
}

fn app(mw: Compression) -> TestClient {
    let text = big_text();
    App::new()
        .wrap(mw)
        .get("/text", move || {
            let t = text.clone();
            async move { t }
        })
        .get("/small", || async { "tiny" })
        .get("/json", || async {
            let body = format!("[{}]", vec![r#"{"id":1,"name":"ada"}"#; 200].join(","));
            (
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )],
                body,
            )
        })
        .test()
}

#[tokio::test]
async fn compresses_with_each_encoding() {
    let c = app(Compression::new());
    for enc in ["gzip", "br", "zstd", "deflate"] {
        let r = c.get("/text").header("accept-encoding", enc).send().await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.header("content-encoding"), Some(enc));
        assert_eq!(r.header("vary"), Some("accept-encoding"));
        assert!(
            r.bytes().len() < big_text().len() / 3,
            "{enc} did not shrink"
        );
        assert_eq!(decode(enc, r.bytes()).await, big_text().as_bytes(), "{enc}");
    }
}

#[tokio::test]
async fn negotiates_by_q_value_and_preference() {
    let c = app(Compression::new());
    let pick = |accept: &'static str| {
        let req = c.get("/text").header("accept-encoding", accept);
        async move {
            req.send()
                .await
                .header("content-encoding")
                .map(str::to_owned)
        }
    };
    assert_eq!(pick("gzip, deflate, br, zstd").await.as_deref(), Some("br"));
    assert_eq!(pick("gzip, deflate, zstd").await.as_deref(), Some("zstd"));
    assert_eq!(pick("deflate, gzip").await.as_deref(), Some("gzip"));
    assert_eq!(pick("br;q=0.2, gzip;q=0.9").await.as_deref(), Some("gzip"));
    assert_eq!(pick("br;q=0, *").await.as_deref(), Some("zstd"));
    assert_eq!(pick("*;q=0").await.as_deref(), None);
    assert_eq!(pick("identity").await.as_deref(), None);
    assert_eq!(pick("compress, x-gzip").await.as_deref(), Some("gzip"));
}

#[tokio::test]
async fn respects_disabled_encodings() {
    let c = app(Compression::new().br(false).zstd(false));
    let r = c
        .get("/text")
        .header("accept-encoding", "br, zstd, gzip")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    let r = c
        .get("/text")
        .header("accept-encoding", "br, zstd")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), None);
    assert_eq!(r.text(), big_text());

    let c = app(Compression::new().gzip(false).br(false).zstd(false));
    let r = c
        .get("/text")
        .header("accept-encoding", "gzip, deflate")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), Some("deflate"));
    assert_eq!(decode("deflate", r.bytes()).await, big_text().as_bytes());
    let c = app(Compression::new().deflate(false));
    let r = c
        .get("/text")
        .header("accept-encoding", "deflate")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), None);
}

#[tokio::test]
async fn without_accept_encoding_sends_identity_but_varies() {
    let c = app(Compression::new());
    let r = c.get("/text").send().await;
    assert_eq!(r.header("content-encoding"), None);
    assert_eq!(r.header("vary"), Some("accept-encoding"));
    assert_eq!(r.text(), big_text());
}

#[tokio::test]
async fn skips_small_bodies() {
    let c = app(Compression::new());
    let r = c
        .get("/small")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), None);
    assert_eq!(r.header("vary"), None);
    assert_eq!(r.text(), "tiny");

    let c = app(Compression::new().min_size(0));
    let r = c
        .get("/small")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(decode("gzip", r.bytes()).await, b"tiny");
}

#[tokio::test]
async fn compresses_json_at_any_level() {
    for level in [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Best,
        CompressionLevel::Precise(3),
    ] {
        let c = app(Compression::new().quality(level));
        for enc in ["br", "gzip", "zstd"] {
            let r = c.get("/json").header("accept-encoding", enc).send().await;
            assert_eq!(r.header("content-encoding"), Some(enc), "{level:?}");
            let body = decode(enc, r.bytes()).await;
            assert!(body.starts_with(br#"[{"id":1"#));
        }
    }
}

fn respond_with<F>(make: F) -> TestClient
where
    F: Fn() -> Response + Clone + Send + Sync + 'static,
{
    App::new()
        .wrap(Compression::new())
        .get("/", move || {
            let make = make.clone();
            async move { make() }
        })
        .test()
}

fn text_response(ct: &'static str) -> Response {
    let mut r = big_text().into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    r
}

async fn encoding_of<F>(make: F) -> Option<String>
where
    F: Fn() -> Response + Clone + Send + Sync + 'static,
{
    respond_with(make)
        .get("/")
        .header("accept-encoding", "gzip, br")
        .send()
        .await
        .header("content-encoding")
        .map(str::to_owned)
}

fn with_header(name: header::HeaderName, value: &'static str) -> impl Fn() -> Response + Clone {
    move || {
        let mut r = text_response("text/plain");
        r.headers_mut()
            .insert(name.clone(), HeaderValue::from_static(value));
        r
    }
}

#[tokio::test]
async fn skip_rules() {
    for ct in [
        "image/png",
        "video/mp4",
        "audio/ogg",
        "font/woff2",
        "application/zip",
        "application/octet-stream",
        "text/event-stream",
    ] {
        assert_eq!(encoding_of(move || text_response(ct)).await, None, "{ct}");
    }
    // Compressible, including parameters and odd casing.
    for ct in [
        "image/svg+xml",
        "application/javascript; charset=utf-8",
        "Text/HTML; charset=UTF-8",
        "application/vnd.api+json",
        "application/wasm",
    ] {
        let enc = encoding_of(move || text_response(ct)).await;
        assert_eq!(enc.as_deref(), Some("br"), "{ct}");
    }
    // No content type: unknown, leave it alone.
    let untyped = || {
        let mut r = text_response("text/plain");
        r.headers_mut().remove(header::CONTENT_TYPE);
        r
    };
    assert_eq!(encoding_of(untyped).await, None);
    // Already encoded: untouched.
    let r = respond_with(with_header(header::CONTENT_ENCODING, "gzip"))
        .get("/")
        .header("accept-encoding", "br")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(r.text(), big_text());
    // no-transform, ranges.
    let no_transform = with_header(header::CACHE_CONTROL, "public, No-Transform");
    assert_eq!(encoding_of(no_transform).await, None);
    let range = with_header(header::CONTENT_RANGE, "bytes 0-9/100");
    assert_eq!(encoding_of(range).await, None);
    // Status codes without a (full) body.
    for status in [
        StatusCode::NO_CONTENT,
        StatusCode::NOT_MODIFIED,
        StatusCode::PARTIAL_CONTENT,
    ] {
        let c = App::new()
            .wrap(Compression::new())
            .get("/", move || async move { (status, big_text()) })
            .test();
        let r = c.get("/").header("accept-encoding", "gzip").send().await;
        assert_eq!(r.header("content-encoding"), None, "{status}");
    }
}

#[tokio::test]
async fn head_requests_are_not_compressed() {
    let c = app(Compression::new());
    let r = c
        .request(Method::HEAD, "/text")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-encoding"), None);
}

#[tokio::test]
async fn fixes_up_entity_headers() {
    let c = App::new()
        .wrap(Compression::new())
        .get("/", || async {
            let body = big_text();
            let len = body.len().to_string();
            let mut r = body.into_response();
            let h = r.headers_mut();
            h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&len).unwrap());
            h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            h.insert(header::ETAG, HeaderValue::from_static("\"v1\""));
            h.insert(header::VARY, HeaderValue::from_static("origin"));
            r
        })
        .get("/weak", || async {
            let mut r = big_text().into_response();
            let h = r.headers_mut();
            h.insert(header::ETAG, HeaderValue::from_static("W/\"v1\""));
            h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
            r
        })
        .test();
    let r = c.get("/").header("accept-encoding", "gzip").send().await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(r.header("content-length"), None);
    assert_eq!(r.header("accept-ranges"), None);
    assert_eq!(r.header("etag"), Some("W/\"v1\""));
    let vary: Vec<_> = r.headers().get_all("vary").iter().collect();
    assert_eq!(vary, ["origin", "accept-encoding"]);

    let r = c
        .get("/weak")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(r.header("etag"), Some("W/\"v1\""));
    assert_eq!(
        r.headers().get_all("vary").iter().count(),
        1,
        "no duplicate vary"
    );
}

fn chunked(chunks: Vec<String>) -> Body {
    let stream = futures_util::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok::<_, Infallible>(Frame::data(Bytes::from(c)))),
    );
    Body::stream(StreamBody::new(stream))
}

#[tokio::test]
async fn compresses_streaming_bodies() {
    let c = App::new()
        .wrap(Compression::new())
        .get("/", || async {
            let chunks = (0..50)
                .map(|i| format!("chunk {i} {}\n", "x".repeat(i)))
                .collect();
            let mut r = Response::new(chunked(chunks));
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            r
        })
        .test();
    let expected: String = (0..50)
        .map(|i| format!("chunk {i} {}\n", "x".repeat(i)))
        .collect();
    for enc in ["gzip", "br", "zstd", "deflate"] {
        let r = c.get("/").header("accept-encoding", enc).send().await;
        assert_eq!(r.header("content-encoding"), Some(enc));
        assert_eq!(decode(enc, r.bytes()).await, expected.as_bytes(), "{enc}");
    }
    // Without negotiation the stream passes through untouched.
    assert_eq!(c.get("/").send().await.text(), expected);
}

#[tokio::test]
async fn streaming_body_errors_propagate() {
    #[derive(Debug)]
    struct Boom;
    impl std::fmt::Display for Boom {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("boom")
        }
    }
    impl std::error::Error for Boom {}

    let c = App::new()
        .wrap(Compression::new())
        .get("/", || async {
            let stream = futures_util::stream::iter([
                Ok(Frame::data(Bytes::from("x".repeat(2000)))),
                Err(Boom),
            ]);
            let mut r = Response::new(Body::stream(StreamBody::new(stream)));
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            r
        })
        .serve_for_test()
        .await;
    let res = c.get("gzip").await;
    assert!(res.is_err(), "a broken stream must not look complete");
}

#[test]
fn default_predicate() {
    assert!(is_compressible("application/json"));
    assert!(!is_compressible("image/webp"));
}

#[tokio::test]
async fn custom_content_type_predicate() {
    let c = App::new()
        .wrap(Compression::new().content_types(|ct| ct == "application/octet-stream"))
        .get("/bin", || async { Bytes::from(vec![7u8; 4096]) })
        .get("/text", || async { big_text() })
        .test();
    let r = c.get("/bin").header("accept-encoding", "gzip").send().await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(decode("gzip", r.bytes()).await, vec![7u8; 4096]);
    let r = c
        .get("/text")
        .header("accept-encoding", "gzip")
        .send()
        .await;
    assert_eq!(r.header("content-encoding"), None);
}

/// Minimal raw-socket harness for streaming checks.
trait ServeForTest {
    async fn serve_for_test(self) -> RawClient;
}

impl ServeForTest for App {
    async fn serve_for_test(self) -> RawClient {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(self.serve(listener));
        RawClient(addr)
    }
}

struct RawClient(std::net::SocketAddr);

type Sender = hyper::client::conn::http1::SendRequest<http_body_util::Empty<Bytes>>;

impl RawClient {
    async fn connect(&self) -> Sender {
        let tcp = tokio::net::TcpStream::connect(self.0).await.unwrap();
        let (sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tcp))
                .await
                .unwrap();
        tokio::spawn(conn);
        sender
    }

    /// GET / and collect the body; `Err` if the body fails mid-stream.
    async fn get(&self, encoding: &str) -> Result<Vec<u8>, hyper::Error> {
        let mut sender = self.connect().await;
        let req = http::Request::get("/")
            .header("accept-encoding", encoding)
            .header("host", "x")
            .body(http_body_util::Empty::new())
            .unwrap();
        let res = sender.send_request(req).await?;
        Ok(res.into_body().collect().await?.to_bytes().to_vec())
    }
}

#[tokio::test]
async fn event_streams_flush_promptly_when_enabled() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();
    let rx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let client = App::new()
        .wrap(
            Compression::new().content_types(|ct| is_compressible(ct) || ct == "text/event-stream"),
        )
        .get("/", move || {
            let rx = rx.clone();
            async move {
                let rx = rx.lock().await.take().expect("single request");
                let events = futures_util::stream::unfold(rx, |mut rx| async move {
                    let ev = rx.recv().await?;
                    Some((Ok::<_, Infallible>(Frame::data(Bytes::from(ev))), rx))
                });
                let mut r = Response::new(Body::stream(StreamBody::new(events)));
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                r
            }
        })
        .serve_for_test()
        .await;

    let mut sender = client.connect().await;
    let req = http::Request::get("/")
        .header("accept-encoding", "gzip")
        .header("host", "x")
        .body(http_body_util::Empty::new())
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.headers()["content-encoding"], "gzip");
    let mut body = res.into_body();

    // Decode incrementally: compressed frames go into a pipe, the decoder
    // reads from the other end.
    let (mut pipe_in, pipe_out) = tokio::io::duplex(64 * 1024);
    let mut decoder = GzipDecoder::new(tokio::io::BufReader::new(pipe_out));
    let mut decoded = Vec::new();

    for event in ["data: one\n\n", "data: two\n\n"] {
        tx.send(event).unwrap();
        let wait = async {
            while !String::from_utf8_lossy(&decoded).ends_with(event) {
                tokio::select! {
                    frame = body.frame() => {
                        let frame = frame.expect("stream ended early").unwrap();
                        if let Ok(data) = frame.into_data() {
                            pipe_in.write_all(&data).await.unwrap();
                        }
                    }
                    n = async {
                        let mut buf = [0u8; 256];
                        let n = decoder.read(&mut buf).await.unwrap();
                        decoded.extend_from_slice(&buf[..n]);
                        n
                    } => { assert!(n > 0, "decoder hit EOF"); }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("{event:?} was not flushed while the stream was open"));
    }
    assert_eq!(
        String::from_utf8_lossy(&decoded),
        "data: one\n\ndata: two\n\n"
    );

    // Closing the stream finishes the gzip member.
    drop(tx);
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            pipe_in.write_all(&data).await.unwrap();
        }
    }
    drop(pipe_in);
    decoder.read_to_end(&mut decoded).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&decoded),
        "data: one\n\ndata: two\n\n"
    );
}

#[tokio::test]
async fn real_socket_full_body_has_content_length() {
    let client = App::new()
        .wrap(Compression::new())
        .get("/", || async { big_text() })
        .serve_for_test()
        .await;
    let mut sender = client.connect().await;
    let req = http::Request::get("/")
        .header("accept-encoding", "br")
        .header("host", "x")
        .body(http_body_util::Empty::new())
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.headers()["content-encoding"], "br");
    let len: usize = res.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), len);
    assert_eq!(decode("br", &body).await, big_text().as_bytes());

    let plain = client.get("identity").await.unwrap();
    assert_eq!(plain, big_text().as_bytes());
}

#[tokio::test]
async fn large_buffered_bodies_are_streamed() {
    let big: String = (0..300_000).map(|i| format!("{i},")).collect();
    assert!(big.len() > 1024 * 1024);
    let body = big.clone();
    let c = App::new()
        .wrap(Compression::new().quality(CompressionLevel::Fastest))
        .get("/", move || {
            let b = body.clone();
            async move { b }
        })
        .test();
    let r = c.get("/").header("accept-encoding", "gzip").send().await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(decode("gzip", r.bytes()).await, big.as_bytes());
}
