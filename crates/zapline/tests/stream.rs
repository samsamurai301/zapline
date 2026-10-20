//! Streaming request bodies (`BodyStream`) and streaming responses
//! (`StreamBody`, `Body::from_stream`, `Ndjson`).

use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use hyper::body::Body as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapline::prelude::*;
use zapline::stream::{BodyStream, Ndjson, StreamBody};
use zapline::Body;

/// Reports `<chunks>:<bytes>`.
async fn count(mut body: BodyStream) -> Result<String> {
    let (mut chunks, mut bytes) = (0, 0);
    while let Some(chunk) = body.chunk().await? {
        chunks += 1;
        bytes += chunk.len();
    }
    assert_eq!(body.bytes_read(), bytes);
    Ok(format!("{chunks}:{bytes}"))
}

#[tokio::test]
async fn body_stream_in_process() {
    let c = App::new().body_limit(10).post("/", count).test();
    assert_eq!(c.post("/").body("hello").send().await.text(), "1:5");
    assert_eq!(c.post("/").send().await.text(), "0:0");
    assert_eq!(c.post("/").body("0123456789").send().await.text(), "1:10");

    let r = c.post("/").body("0123456789!").send().await;
    assert_eq!(r.status(), 413);
}

#[tokio::test]
async fn body_stream_limit_override_and_stream_trait() {
    async fn big(body: BodyStream) -> Result<String> {
        let mut body = body.with_limit(1000);
        assert_eq!(body.limit(), 1000);
        let mut n = 0;
        // `BodyStream` is a `Stream` too.
        while let Some(chunk) = body.next().await {
            n += chunk?.len();
        }
        Ok(n.to_string())
    }
    let c = App::new().body_limit(10).post("/", big).test();
    assert_eq!(c.post("/").body(vec![b'x'; 500]).send().await.text(), "500");
    assert_eq!(
        c.post("/").body(vec![b'x'; 1001]).send().await.status(),
        413
    );
}

async fn serve(app: App) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

async fn raw(addr: SocketAddr, request: &[u8]) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .unwrap()
        .ok();
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn chunked_upload_is_streamed_and_limited_across_chunks() {
    let addr = serve(App::new().body_limit(12).post("/", count)).await;

    // Three chunks of 4 bytes: within the limit, delivered chunk by chunk.
    let ok = raw(
        addr,
        b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
          4\r\naaaa\r\n4\r\nbbbb\r\n4\r\ncccc\r\n0\r\n\r\n",
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(ok.ends_with(":12"), "{ok}");

    // No Content-Length: the limit trips in the middle of the stream.
    let too_big = raw(
        addr,
        b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
          8\r\naaaaaaaa\r\n8\r\nbbbbbbbb\r\n0\r\n\r\n",
    )
    .await;
    assert!(too_big.starts_with("HTTP/1.1 413"), "{too_big}");

    // A declared Content-Length over the limit fails on the first read.
    let declared = raw(
        addr,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(declared.starts_with("HTTP/1.1 413"), "{declared}");
}

/// Drain a response body, recording a terminal error.
async fn frames(res: Response) -> (Vec<Bytes>, Option<String>) {
    let mut body = res.into_body();
    let mut out = Vec::new();
    loop {
        match std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            None => return (out, None),
            Some(Ok(f)) => out.extend(f.into_data().ok()),
            Some(Err(e)) => return (out, Some(e.to_string())),
        }
    }
}

#[tokio::test]
async fn stream_body_responses() {
    let c = App::new()
        .get("/csv", || async {
            let rows =
                stream::iter(["id,name\n", "1,ada\n", "", "2,bob\n"]).map(Ok::<_, std::io::Error>);
            StreamBody::new(rows).content_type("text/csv")
        })
        .get("/bin", || async {
            StreamBody::new(stream::iter(vec![
                Ok::<_, std::io::Error>(vec![1u8, 2]),
                Ok(vec![3]),
            ]))
        })
        .get("/bad-type", || async {
            StreamBody::new(stream::iter([Ok::<_, std::io::Error>("x")])).content_type("bad\nvalue")
        })
        .test();

    let r = c.get("/csv").send().await;
    assert_eq!(r.header("content-type"), Some("text/csv"));
    assert_eq!(r.text(), "id,name\n1,ada\n2,bob\n");

    let r = c.get("/bin").send().await;
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert_eq!(r.bytes().as_ref(), &[1, 2, 3]);

    assert_eq!(c.get("/bad-type").send().await.status(), 500);
}

#[tokio::test]
async fn body_from_stream_skips_empty_chunks_and_propagates_errors() {
    let s = stream::iter(vec![
        Ok::<_, std::io::Error>(Bytes::from_static(b"a")),
        Ok(Bytes::new()),
        Ok(Bytes::from_static(b"b")),
        Err(std::io::Error::other("disk on fire")),
        Ok(Bytes::from_static(b"never")),
    ]);
    let (chunks, err) = frames(Response::new(Body::from_stream(s))).await;
    assert_eq!(chunks, vec!["a", "b"]);
    assert_eq!(err.as_deref(), Some("disk on fire"));
}

#[tokio::test]
async fn mid_stream_error_aborts_the_connection() {
    let addr = serve(App::new().get("/", || async {
        let s = stream::iter(vec![Ok("partial"), Err(std::io::Error::other("boom"))]);
        Response::new(Body::from_stream(s))
    }))
    .await;
    let out = raw(addr, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    // The connection is closed without the terminating zero-length chunk,
    // so the client can tell the response is incomplete. (Depending on
    // buffering, the head and first chunk may or may not have been flushed.)
    assert!(out.is_empty() || out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(!out.contains("0\r\n\r\n"), "{out}");
}

#[tokio::test]
async fn ndjson_streams_one_document_per_line() {
    #[derive(serde::Serialize)]
    struct Row {
        id: u32,
        name: &'static str,
    }
    let c = App::new()
        .get("/rows", || async {
            Ndjson::new(stream::iter([
                Row { id: 1, name: "ada" },
                Row { id: 2, name: "bob" },
            ]))
        })
        .test();
    let r = c.get("/rows").send().await;
    assert_eq!(r.header("content-type"), Some("application/x-ndjson"));
    assert_eq!(
        r.text(),
        "{\"id\":1,\"name\":\"ada\"}\n{\"id\":2,\"name\":\"bob\"}\n"
    );

    let fallible = stream::iter(vec![
        Ok(serde_json::json!({"n": 1})),
        Err(std::io::Error::other("query failed")),
    ]);
    let (chunks, err) = frames(Ndjson::try_new(fallible).into_response()).await;
    assert_eq!(chunks, vec!["{\"n\":1}\n"]);
    assert_eq!(err.as_deref(), Some("query failed"));
}

#[tokio::test]
async fn request_stream_piped_into_response() {
    // `BodyStream` yields `zapline::Error`, which `StreamBody` accepts.
    async fn echo(body: BodyStream) -> StreamBody<BodyStream> {
        StreamBody::new(body).content_type("text/plain")
    }
    let addr = serve(App::new().body_limit(8).post("/echo", echo)).await;
    let out = raw(
        addr,
        b"POST /echo HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 5\r\n\r\nhello",
    )
    .await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("hello"), "{out}");

    let c = App::new().post("/echo", echo).test();
    assert_eq!(c.post("/echo").body("ping").send().await.text(), "ping");

    // The error is preserved through the response body.
    let s = stream::iter(vec![
        Ok(Bytes::from_static(b"x")),
        Err(Error::payload_too_large()),
    ]);
    let (chunks, err) = frames(StreamBody::new(s).into_response()).await;
    assert_eq!(chunks, vec!["x"]);
    assert_eq!(err.as_deref(), Some("payload too large"));
}
