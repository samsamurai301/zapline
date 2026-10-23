//! Server-Sent Events: wire format, headers, error handling and keep-alive.
#![cfg(feature = "json")]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use futures_util::stream::{self, Stream, StreamExt};
use hyper::body::Body as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zapline::prelude::*;
use zapline::sse::{Event, KeepAlive, Sse};

/// Drain a response body frame by frame, recording a terminal error.
async fn frames(res: Response) -> (Vec<Bytes>, Option<String>) {
    let mut body = res.into_body();
    let mut out = Vec::new();
    loop {
        match std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            None => return (out, None),
            Some(Ok(f)) => {
                if let Ok(d) = f.into_data() {
                    out.push(d);
                }
            }
            Some(Err(e)) => return (out, Some(e.to_string())),
        }
    }
}

#[tokio::test]
async fn finite_stream_exact_wire_format() {
    async fn events() -> Sse<impl Stream<Item = Event>> {
        Sse::new(stream::iter(vec![
            Event::default().data("hello"),
            Event::default()
                .event("update")
                .id("1")
                .data("line1\nline2"),
            Event::default().retry(Duration::from_millis(1500)),
            Event::default().comment("just a comment"),
            Event::default()
                .json_data(serde_json::json!({"a": 1}))
                .unwrap(),
            Event::default().data("crlf\r\nand\rcr\n"),
            Event::default().data(""),
            Event::default().comment("two\nlines").data("x"),
        ]))
    }
    let c = App::new().get("/events", events).test();
    let r = c.get("/events").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert_eq!(r.header("x-accel-buffering"), Some("no"));
    assert_eq!(
        r.text(),
        concat!(
            "data: hello\n\n",
            "event: update\nid: 1\ndata: line1\ndata: line2\n\n",
            "retry: 1500\n\n",
            ":just a comment\n\n",
            "data: {\"a\":1}\n\n",
            "data: crlf\ndata: and\ndata: cr\ndata: \n\n",
            "data: \n\n",
            ":two\n:lines\ndata: x\n\n",
        )
    );
}

#[tokio::test]
async fn result_streams_and_errors() {
    // `Result<Event, E>` items are accepted; `Ok`s are sent in order.
    let ok = stream::iter(vec![
        Ok::<_, Infallible>(Event::default().data("1")),
        Ok(Event::default().data("2")),
    ]);
    let (chunks, err) = frames(Sse::new(ok).into_response()).await;
    assert_eq!(chunks, vec!["data: 1\n\n", "data: 2\n\n"]);
    assert!(err.is_none());

    // An `Err` ends the stream with that error.
    let failing = stream::iter(vec![
        Ok(Event::default().data("1")),
        Err(std::io::Error::other("database went away")),
        Ok(Event::default().data("never sent")),
    ]);
    let (chunks, err) = frames(Sse::new(failing).into_response()).await;
    assert_eq!(chunks, vec!["data: 1\n\n"]);
    assert_eq!(err.as_deref(), Some("database went away"));
}

#[tokio::test]
async fn invalid_fields_end_the_stream_instead_of_injecting() {
    let bad_event = Event::default().event("evil\ndata: injected").data("x");
    assert!(!bad_event.is_valid());
    assert!(bad_event.to_bytes().is_err());
    assert!(!Event::default().id("1\r2").is_valid());
    assert!(!Event::default().id("nul\0").is_valid());
    assert!(Event::default().id("ok").event("fine").is_valid());

    let s = stream::iter(vec![Event::default().data("before"), bad_event]);
    let (chunks, err) = frames(Sse::new(s).into_response()).await;
    assert_eq!(chunks, vec!["data: before\n\n"]);
    assert!(err.unwrap().contains("line breaks"));

    // Data and comments can never inject fields: line breaks become new
    // `data:`/comment lines.
    let ev = Event::default()
        .data("a\nevent: injected")
        .to_bytes()
        .unwrap();
    assert_eq!(ev, "data: a\ndata: event: injected\n\n");
}

#[test]
fn event_to_bytes() {
    let ev = Event::default()
        .id("42")
        .event("tick")
        .data("{}")
        .retry(Duration::from_secs(2));
    assert_eq!(
        ev.to_bytes().unwrap(),
        "id: 42\nevent: tick\ndata: {}\nretry: 2000\n\n"
    );
    assert_eq!(Event::default().to_bytes().unwrap(), "\n");
    let dbg = format!("{ev:?}");
    assert!(dbg.contains("tick"), "{dbg}");
}

async fn serve(app: App) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

/// Open a raw connection and read until `pred(buffer)` holds.
async fn read_until(addr: SocketAddr, path: &str, pred: impl Fn(&str) -> bool) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = s.read(&mut buf).await.unwrap();
            assert!(
                n > 0,
                "connection closed: {}",
                String::from_utf8_lossy(&out)
            );
            out.extend_from_slice(&buf[..n]);
            if pred(&String::from_utf8_lossy(&out)) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; got {}", String::from_utf8_lossy(&out)));
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn keep_alive_on_idle_stream_over_socket() {
    let app = App::new().get("/idle", || async {
        Sse::new(stream::pending::<Event>()).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_millis(30))
                .text("keep-alive"),
        )
    });
    let addr = serve(app).await;
    let out = read_until(addr, "/idle", |s| s.matches(":keep-alive\n\n").count() >= 3).await;
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
    assert!(out.contains("content-type: text/event-stream"), "{out}");
    assert!(out.contains("cache-control: no-cache"), "{out}");
    assert!(out.contains("x-accel-buffering: no"), "{out}");
    assert!(out.contains("transfer-encoding: chunked"), "{out}");
}

#[tokio::test]
async fn keep_alive_interleaves_with_events() {
    let app = App::new().get("/slow", || async {
        // One event every 120 ms; keep-alives every 30 ms in between.
        let events = stream::unfold(0u32, |n| async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            Some((Event::default().id(n.to_string()).data("tick"), n + 1))
        })
        .take(2);
        Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_millis(30)))
    });
    let addr = serve(app).await;
    let out = read_until(addr, "/slow", |s| s.contains("id: 1\ndata: tick\n\n")).await;
    let first_event = out.find("id: 0\n").unwrap();
    let first_keep_alive = out.find(":\n\n").expect("no keep-alive frame");
    assert!(first_keep_alive < first_event, "{out}");
    assert!(out[first_event..].contains(":\n\n"), "{out}");
}

#[tokio::test]
async fn default_keep_alive_and_text_sanitizing() {
    let s = Sse::new(stream::pending::<Event>()).keep_alive(
        KeepAlive::default()
            .interval(Duration::ZERO) // clamped to 1 ms
            .text("a\nb"),
    );
    let mut body = s.into_response().into_body();
    let frame = tokio::time::timeout(
        Duration::from_secs(2),
        std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(frame.into_data().unwrap(), ":a\n:b\n\n");
}

#[tokio::test]
async fn zapline_errors_in_event_streams() {
    // `json_data` returns `zapline::Error`, so `Result<Event, Error>` streams
    // are the natural shape.
    async fn feed() -> Sse<impl Stream<Item = Result<Event>>> {
        Sse::new(stream::iter(vec![
            Event::default().json_data([1, 2]),
            Err(Error::service_unavailable("upstream down")),
        ]))
    }
    let (chunks, err) = frames(feed().await.into_response()).await;
    assert_eq!(chunks, vec!["data: [1,2]\n\n"]);
    assert_eq!(err.as_deref(), Some("upstream down"));
}
