//! Signed webhooks end to end: a WebhookSender delivering to an app that
//! verifies with `Signed<T>`, retries with one message id, refused URLs,
//! and the extractor's rejections.
#![cfg(feature = "webhooks")]

mod server_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use server_support::TestServer;
use zapline::outbound::HttpClient;
use zapline::prelude::*;
use zapline::webhooks::{Signed, Webhook, WebhookSender};

async fn serve(app: App) -> (TestServer, String) {
    let mut app = app;
    app.server_config_mut().banner = false;
    let server = TestServer::spawn(app).await;
    let base = format!("http://{}", server.addr);
    (server, base)
}

/// A sender that may reach the local test servers, retrying quickly.
fn sender(secret: &str) -> WebhookSender {
    WebhookSender::new(secret)
        .unwrap()
        .http_client(HttpClient::new().allow_private_networks(true))
        .backoff(Duration::from_millis(10))
}

/// A receiver whose `/hook` answers `fail_first` times with 503, then
/// records the event and answers 204.
type Ids = Arc<Mutex<Vec<String>>>;

async fn receiver(
    secret: &str,
    fail_first: usize,
) -> (TestServer, String, Arc<Mutex<Vec<(String, Value)>>>, Ids) {
    let got = Arc::new(Mutex::new(Vec::new()));
    let log = got.clone();
    let hits = Arc::new(AtomicUsize::new(0));
    let ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_ids = ids.clone();
    let app = App::new()
        .post("/hook", move |req: Request| {
            // Record the id of every attempt before verifying anything.
            let id = req
                .headers()
                .get("webhook-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            seen_ids.lock().unwrap().push(id);
            let n = hits.fetch_add(1, Ordering::SeqCst);
            let log = log.clone();
            let seen_ids = seen_ids.clone();
            async move {
                if n < fail_first {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                let id = seen_ids.lock().unwrap().last().cloned().unwrap_or_default();
                let res = verify_and_parse(req).await;
                match res {
                    Ok(Signed(event)) => {
                        log.lock().unwrap().push((id, event));
                        StatusCode::NO_CONTENT.into_response()
                    }
                    Err(e) => e.into_response(),
                }
            }
        })
        .state(Webhook::standard(secret).unwrap());
    let (server, base) = serve(app).await;
    (server, format!("{base}/hook"), got, ids)
}

async fn verify_and_parse(req: Request) -> Result<Signed<Value>> {
    use zapline::extract::FromRequest;
    Signed::<Value>::from_request(req).await
}

#[tokio::test]
async fn a_signed_webhook_is_delivered_and_verified() {
    let secret = WebhookSender::generate_secret();
    let (_s, url, got, _) = receiver(&secret, 0).await;
    let delivery = sender(&secret)
        .send(&url, &json!({"type": "invoice.paid", "amount": 42}))
        .await
        .unwrap();
    assert_eq!(delivery.status, StatusCode::NO_CONTENT);
    assert_eq!(delivery.attempts, 1);
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, delivery.id);
    assert_eq!(got[0].1["amount"], 42);
}

#[tokio::test]
async fn failures_are_retried_with_the_same_message_id() {
    let secret = WebhookSender::generate_secret();
    let (_s, url, got, ids) = receiver(&secret, 2).await;
    let delivery = sender(&secret).send(&url, &json!({"n": 1})).await.unwrap();
    assert_eq!(delivery.attempts, 3);
    // Every attempt carried the same id, the one the sender reports.
    assert_eq!(*ids.lock().unwrap(), vec![delivery.id.clone(); 3]);
    assert_eq!(got.lock().unwrap()[0].0, delivery.id);
}

#[tokio::test]
async fn retries_give_up_and_say_why() {
    let secret = WebhookSender::generate_secret();
    let (_s, url, got, _) = receiver(&secret, 100).await;
    let err = sender(&secret)
        .retries(2)
        .send(&url, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(err.code(), Some("webhook_delivery_failed"));
    assert!(err.to_string().contains("3 attempt"), "{err}");
    assert!(err.to_string().contains("503"), "{err}");
    assert!(got.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_wrong_secret_is_rejected_and_not_retried() {
    let (_s, url, got, _) = receiver(&WebhookSender::generate_secret(), 0).await;
    let err = sender(&WebhookSender::generate_secret())
        .send(&url, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("webhook_delivery_failed"));
    assert!(err.to_string().contains("1 attempt"), "{err}");
    assert!(err.to_string().contains("401"), "{err}");
    assert!(got.lock().unwrap().is_empty());
}

#[tokio::test]
async fn private_addresses_are_refused_by_default() {
    let plain = WebhookSender::new(&WebhookSender::generate_secret()).unwrap();
    for url in ["http://127.0.0.1:9/hook", "http://169.254.169.254/latest"] {
        let err = plain.send(url, &json!({})).await.unwrap_err();
        assert_eq!(err.code(), Some("upstream_blocked"), "{url}");
    }
}

#[tokio::test]
async fn the_extractor_rejects_unsigned_and_malformed_requests() {
    let secret = WebhookSender::generate_secret();
    let c = App::new()
        .post("/hook", |Signed(v): Signed<Value>| async move { Json(v) })
        .meta(Webhook::standard(&secret).unwrap())
        .post("/unconfigured", |Signed(v): Signed<Value>| async move {
            Json(v)
        })
        .test();

    let r = c.post("/hook").json(&json!({"a": 1})).send().await;
    assert_eq!(r.status(), 400);
    assert!(
        r.text().contains("webhook_signature_missing"),
        "{}",
        r.text()
    );

    let r = c
        .post("/hook")
        .header("webhook-id", "msg_1")
        .header("webhook-timestamp", "1")
        .header("webhook-signature", "v1,AAAA")
        .json(&json!({"a": 1}))
        .send()
        .await;
    assert_eq!(r.status(), 401);
    assert!(r.text().contains("webhook_timestamp_stale"), "{}", r.text());

    assert_eq!(c.post("/unconfigured").send().await.status(), 500);
}
