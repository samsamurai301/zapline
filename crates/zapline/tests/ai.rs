//! The LLM client against fake Anthropic and OpenAI-compatible servers on
//! real sockets: requests, replies, streams, SSE relay, retries, refusals.
#![cfg(feature = "ai")]

mod server_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde_json::{json, Value};
use server_support::TestServer;
use zapline::ai::{Effort, Llm, StopReason};
use zapline::outbound::HttpClient;
use zapline::prelude::*;
use zapline::Body;

/// Requests the fake provider saw: (headers as `name: value` lines, body).
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

async fn serve(app: App) -> String {
    let mut app = app;
    app.server_config_mut().banner = false;
    let server = TestServer::spawn(app).await;
    let base = format!("http://{}", server.addr);
    // Keep the server running for the rest of the test.
    std::mem::forget(server);
    base
}

fn local(llm: Llm, base: &str) -> Llm {
    llm.base_url(base)
        .http_client(HttpClient::new().allow_private_networks(true))
}

fn record(seen: &Seen, req: &Request, body: &str) {
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n");
    seen.lock()
        .unwrap()
        .push((headers, serde_json::from_str(body).unwrap_or(Value::Null)));
}

fn sse(events: &[(&str, Value)]) -> Response {
    let mut text = String::new();
    for (name, data) in events {
        text.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    }
    let mut res = Response::new(Body::from(text));
    res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    res
}

/// A fake Messages API: a whole reply, or a stream of `pieces`.
async fn anthropic(pieces: &'static [&'static str], stop: &'static str) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let base = serve(App::new().post("/v1/messages", move |mut req: Request| {
        let log = log.clone();
        async move {
            let body = String::from_utf8(req.body_bytes().await.unwrap().to_vec()).unwrap();
            record(&log, &req, &body);
            let v: Value = serde_json::from_str(&body).unwrap();
            if v["stream"] == true {
                let mut events = vec![(
                    "message_start",
                    json!({"type": "message_start", "message": {"usage": {"input_tokens": 12}}}),
                )];
                for p in pieces {
                    events.push((
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": 0,
                               "delta": {"type": "text_delta", "text": p}}),
                    ));
                }
                events.push((
                    "message_delta",
                    json!({"type": "message_delta", "delta": {"stop_reason": stop},
                           "usage": {"output_tokens": 7}}),
                ));
                events.push(("message_stop", json!({"type": "message_stop"})));
                sse(&events)
            } else {
                Json(json!({
                    "model": v["model"],
                    "content": [
                        {"type": "thinking", "thinking": ""},
                        {"type": "text", "text": pieces.concat()},
                    ],
                    "stop_reason": stop,
                    "usage": {"input_tokens": 12, "output_tokens": 7},
                }))
                .into_response()
            }
        }
    }))
    .await;
    (base, seen)
}

#[tokio::test]
async fn anthropic_reply_and_request_shape() {
    let (base, seen) = anthropic(&["Hello", " there"], "end_turn").await;
    let llm = local(Llm::anthropic("sk-test"), &base).effort(Effort::Low);
    let reply = llm
        .chat()
        .system("be brief")
        .user("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.text, "Hello there", "thinking blocks are skipped");
    assert_eq!(reply.stop, StopReason::EndTurn);
    assert_eq!(
        (reply.usage.input_tokens, reply.usage.output_tokens),
        (12, 7)
    );
    assert_eq!(reply.model, "claude-opus-5-5");

    let (headers, body) = seen.lock().unwrap()[0].clone();
    assert!(headers.contains("x-api-key: sk-test"), "{headers}");
    assert!(
        headers.contains("anthropic-version: 2026-10-21"),
        "{headers}"
    );
    assert!(
        headers.contains("anthropic-beta: server-side-fallback-2026-07-01"),
        "{headers}"
    );
    assert_eq!(body["system"], "be brief");
    assert_eq!(body["fallbacks"], "default");
    assert_eq!(body["output_config"]["effort"], "low");
    assert_eq!(body["max_tokens"], 16_000);
}

#[tokio::test]
async fn anthropic_stream_and_refusal() {
    let (base, _) = anthropic(&["one ", "two"], "refusal").await;
    let llm = local(Llm::anthropic("k"), &base);
    let mut tokens = llm.chat().user("hi").stream().await.unwrap();
    let mut out = Vec::new();
    while let Some(t) = tokens.next().await.unwrap() {
        out.push(t);
    }
    assert_eq!(out, ["one ", "two"]);
    assert_eq!(tokens.stop_reason(), Some(&StopReason::Refusal));
    assert_eq!(tokens.usage().input_tokens, 12);
    assert_eq!(tokens.usage().output_tokens, 7);
}

#[tokio::test]
async fn streams_relay_to_the_browser_as_sse() {
    let (base, _) = anthropic(&["Hel", "lo"], "end_turn").await;
    let llm = local(Llm::anthropic("k"), &base);
    let c = App::new()
        .state(llm)
        .get("/ask", |llm: Llm| async move {
            Ok::<_, Error>(llm.chat().user("hi").stream().await?.into_sse())
        })
        .test();
    let r = c.get("/ask").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
    let text = r.text();
    assert!(text.contains("event: token\ndata: Hel\n\n"), "{text}");
    assert!(text.contains("event: token\ndata: lo\n\n"), "{text}");
    assert!(
        text.contains(r#"data: {"input_tokens":12,"output_tokens":7,"stop":"end_turn"}"#),
        "{text}"
    );
}

#[tokio::test]
async fn openai_compatible_reply_and_stream() {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let base = serve(
        App::new().post("/v1/chat/completions", move |mut req: Request| {
            let log = log.clone();
            async move {
                let body = String::from_utf8(req.body_bytes().await.unwrap().to_vec()).unwrap();
                record(&log, &req, &body);
                let v: Value = serde_json::from_str(&body).unwrap();
                if v["stream"] == true {
                    let text = [
                    json!({"choices": [{"delta": {"content": "Hi"}}]}).to_string(),
                    json!({"choices": [{"delta": {"content": "!"}, "finish_reason": "stop"}]})
                        .to_string(),
                    json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}})
                        .to_string(),
                    "[DONE]".to_owned(),
                ]
                .iter()
                .map(|d| format!("data: {d}\n\n"))
                .collect::<String>();
                    Response::new(Body::from(text))
                } else {
                    Json(json!({
                        "model": "llama",
                        "choices": [{"message": {"content": "Hi!"}, "finish_reason": "length"}],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 2},
                    }))
                    .into_response()
                }
            }
        }),
    )
    .await;
    let llm = Llm::openai_compatible(format!("{base}/v1"), "ok-key", "llama")
        .http_client(HttpClient::new().allow_private_networks(true));

    let reply = llm.chat().system("s").user("u").send().await.unwrap();
    assert_eq!(reply.text, "Hi!");
    assert_eq!(reply.stop, StopReason::MaxTokens);
    assert_eq!(reply.usage.output_tokens, 2);
    let (headers, body) = seen.lock().unwrap()[0].clone();
    assert!(
        headers.contains("authorization: Bearer ok-key"),
        "{headers}"
    );
    assert_eq!(body["messages"][0]["role"], "system");

    let mut tokens = llm.chat().user("u").stream().await.unwrap();
    let mut out = String::new();
    while let Some(t) = tokens.next().await.unwrap() {
        out.push_str(&t);
    }
    assert_eq!(out, "Hi!");
    assert_eq!(tokens.stop_reason(), Some(&StopReason::EndTurn));
    assert_eq!(tokens.usage().input_tokens, 3);
}

/// A provider that answers `status` `fails` times, then succeeds.
async fn flaky(status: u16, fails: usize) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let base = serve(App::new().post("/v1/messages", move || {
        let h = h.clone();
        async move {
            if h.fetch_add(1, Ordering::SeqCst) < fails {
                let mut res = (
                    StatusCode::from_u16(status).unwrap(),
                    Json(json!({"type": "error", "error": {"type": "x", "message": "busy right now"}})),
                )
                    .into_response();
                res.headers_mut()
                    .insert("retry-after", HeaderValue::from_static("0"));
                res
            } else {
                Json(json!({"model": "m", "content": [{"type": "text", "text": "ok"}],
                            "stop_reason": "end_turn", "usage": {}}))
                .into_response()
            }
        }
    }))
    .await;
    (base, hits)
}

#[tokio::test]
async fn overloaded_and_rate_limited_requests_are_retried() {
    let (base, hits) = flaky(529, 2).await;
    let reply = local(Llm::anthropic("k"), &base)
        .chat()
        .user("x")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.text, "ok");
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    let (base, hits) = flaky(429, 10).await;
    let err = local(Llm::anthropic("k"), &base)
        .retries(1)
        .chat()
        .user("x")
        .send()
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(err.code(), Some("llm_rate_limited"));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn bad_requests_are_not_retried_and_say_why() {
    let (base, hits) = flaky(400, 10).await;
    let err = local(Llm::anthropic("k"), &base)
        .chat()
        .user("x")
        .send()
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(err.code(), Some("llm_error"));
    assert!(err.to_string().contains("busy right now"), "{err}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_error_event_mid_stream_fails_the_stream() {
    let base = serve(App::new().post("/v1/messages", || async {
        sse(&[
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "par"}}),
            ),
            (
                "error",
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            ),
        ])
    }))
    .await;
    let mut tokens = local(Llm::anthropic("k"), &base)
        .chat()
        .user("x")
        .stream()
        .await
        .unwrap();
    // The text before the error is delivered, then the error.
    assert_eq!(tokens.next().await.unwrap().as_deref(), Some("par"));
    let err = tokens.next().await.unwrap_err();
    assert_eq!(err.code(), Some("llm_error"));
    assert!(err.to_string().contains("Overloaded"), "{err}");
}

#[tokio::test]
async fn a_chat_needs_a_message_and_handlers_need_a_registered_llm() {
    let err = Llm::anthropic("k").chat().send().await.unwrap_err();
    assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let c = App::new().get("/", |_llm: Llm| async { "never" }).test();
    assert_eq!(c.get("/").send().await.status(), 500);
    let _ = Bytes::new();
}
