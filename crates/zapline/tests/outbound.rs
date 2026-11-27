//! The outbound HTTP client against real local servers: the SSRF guard,
//! JSON, retries, timeouts, size limits, streaming and errors.
#![cfg(feature = "http-client")]

mod server_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde_json::{json, Value};
use server_support::TestServer;
use zapline::outbound::HttpClient;
use zapline::prelude::*;
use zapline::Body;

async fn serve(app: App) -> (TestServer, String) {
    let mut app = app;
    app.server_config_mut().banner = false;
    let server = TestServer::spawn(app).await;
    let base = format!("http://{}", server.addr);
    (server, base)
}

/// A client for the local test servers, which are on a private network.
fn local() -> HttpClient {
    HttpClient::new().allow_private_networks(true)
}

fn code(e: &Error) -> String {
    format!("{} {}", e.status(), e.code().unwrap_or(""))
}

#[tokio::test]
async fn private_and_reserved_addresses_are_refused_by_default() {
    let http = HttpClient::new().retries(0);
    for url in [
        "http://127.0.0.1:9/",
        "http://localhost:9/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://[::1]:9/",
        "http://[::ffff:192.168.0.1]/",
    ] {
        let err = http.get(url).send().await.unwrap_err();
        assert_eq!(
            code(&err),
            "400 Bad Request upstream_blocked",
            "{url}: {err}"
        );
    }
    // Only absolute http(s) URLs.
    for url in ["file:///etc/passwd", "/relative", "ftp://example.com/"] {
        let err = http.get(url).send().await.unwrap_err();
        assert_eq!(code(&err), "400 Bad Request upstream_invalid_url", "{url}");
    }
}

#[tokio::test]
async fn json_in_json_out_with_headers() {
    let (_s, base) = serve(App::new().post("/echo", |req: Request| async move {
        let h = |n: &str| {
            req.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned()
        };
        Json(json!({
            "auth": h("authorization"),
            "key": h("x-api-key"),
            "agent": h("user-agent"),
            "type": h("content-type"),
        }))
    }))
    .await;
    let http = local().header("x-api-key", "k-1");
    let body: Value = http
        .post(&format!("{base}/echo"))
        .bearer("t0k")
        .json(&json!({"a": 1}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["auth"], "Bearer t0k");
    assert_eq!(body["key"], "k-1");
    assert!(body["agent"].as_str().unwrap().starts_with("zapline/"));
    assert_eq!(body["type"], "application/json");
}

/// A server that answers 503 (with `Retry-After: 0`) `failures` times first.
async fn flaky(failures: usize) -> (TestServer, String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let handler = move || {
        let h = h.clone();
        async move {
            if h.fetch_add(1, Ordering::SeqCst) < failures {
                Err(Error::service_unavailable("busy")
                    .with_header(zapline::header::RETRY_AFTER, HeaderValue::from_static("0")))
            } else {
                Ok("ok")
            }
        }
    };
    let (s, base) = serve(App::new().get("/", handler.clone()).post("/", handler)).await;
    (s, base, hits)
}

#[tokio::test]
async fn idempotent_requests_are_retried() {
    let (_s, base, hits) = flaky(2).await;
    let res = local().get(&base).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "ok");
    assert_eq!(hits.load(Ordering::SeqCst), 3, "two retries");
}

#[tokio::test]
async fn post_is_never_retried() {
    let (_s, base, hits) = flaky(2).await;
    let res = local().post(&base).send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let err = res.error_for_status().await.unwrap_err();
    assert_eq!(code(&err), "502 Bad Gateway upstream_status");
    assert!(err.to_string().contains("503"), "{err}");
}

#[tokio::test]
async fn retries_give_up_after_the_limit() {
    let (_s, base, hits) = flaky(10).await;
    let res = local().retries(1).get(&base).send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn slow_servers_time_out_with_504() {
    let (_s, base) = serve(App::new().get("/", || async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        "late"
    }))
    .await;
    let err = local()
        .retries(0)
        .timeout(Duration::from_millis(200))
        .get(&base)
        .send()
        .await
        .unwrap_err();
    assert_eq!(code(&err), "504 Gateway Timeout upstream_timeout");
}

#[tokio::test]
async fn unreachable_servers_are_502() {
    let err = local()
        .retries(0)
        .get("http://127.0.0.1:1/")
        .send()
        .await
        .unwrap_err();
    assert_eq!(code(&err), "502 Bad Gateway upstream_unreachable");
}

#[tokio::test]
async fn buffered_bodies_are_capped() {
    let (_s, base) = serve(App::new().get("/", || async { "x".repeat(10_000) })).await;
    let err = local()
        .max_response_size(1_000)
        .get(&base)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap_err();
    assert_eq!(code(&err), "502 Bad Gateway upstream_too_large");
}

#[tokio::test]
async fn bad_json_is_502() {
    let (_s, base) = serve(App::new().get("/", || async { "not json" })).await;
    let err = local()
        .get(&base)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap_err();
    assert_eq!(code(&err), "502 Bad Gateway upstream_invalid_json");
}

#[tokio::test]
async fn streams_arrive_chunk_by_chunk() {
    let (_s, base) = serve(App::new().get("/", || async {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        tokio::spawn(async move {
            for word in ["one ", "two ", "three"] {
                tokio::time::sleep(Duration::from_millis(30)).await;
                let _ = tx.send(Ok(Bytes::from(word))).await;
            }
        });
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Body::stream(http_body_util::StreamBody::new(
            futures_util::StreamExt::map(stream, |r| r.map(http_body::Frame::data)),
        ))
    }))
    .await;
    let mut res = local().get(&base).send().await.unwrap();
    let mut chunks = Vec::new();
    while let Some(chunk) = res.chunk().await.unwrap() {
        chunks.push(String::from_utf8_lossy(&chunk).into_owned());
    }
    assert_eq!(chunks.concat(), "one two three");
    assert!(chunks.len() >= 2, "arrived in pieces: {chunks:?}");
}

#[tokio::test]
async fn handlers_get_the_registered_client() {
    let (_s, upstream) = serve(App::new().get("/", || async { "upstream says hi" })).await;
    let c = App::new()
        .state(local())
        .get("/proxy", move |http: HttpClient| {
            let url = upstream.clone();
            async move { http.get(&url).send().await?.text().await }
        })
        .test();
    let r = c.get("/proxy").send().await;
    assert_eq!(r.status(), 200, "{}", r.text());
    assert_eq!(r.text(), "upstream says hi");

    // Without one registered, the shared default refuses private addresses.
    let c = App::new()
        .get("/proxy", |http: HttpClient| async move {
            http.get("http://127.0.0.1:9/").send().await?.text().await
        })
        .test();
    assert_eq!(c.get("/proxy").send().await.status(), 400);
}
