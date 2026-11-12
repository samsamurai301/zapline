//! Captcha verification against a fake provider on a real socket: what is
//! sent, accepted and rejected tokens, and failing closed.
#![cfg(all(feature = "captcha", feature = "form"))]

mod server_support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use server_support::TestServer;
use zapline::captcha::Captcha;
use zapline::prelude::*;

/// A provider double: accepts the token "good", rejects anything else,
/// and answers "broken" with JSON that has no `success`.
async fn provider() -> (TestServer, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let mut app = App::new().post("/siteverify", move |body: String| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(body.clone());
            if body.contains("response=good") {
                r#"{"success":true}"#
            } else if body.contains("response=broken") {
                r#"{"oops":1}"#
            } else {
                r#"{"success":false,"error-codes":["invalid-input-response"]}"#
            }
        }
    });
    app.server_config_mut().banner = false;
    (TestServer::spawn(app).await, seen)
}

#[derive(Deserialize)]
struct Signup {
    #[serde(rename = "cf-turnstile-response", default)]
    captcha: String,
}

fn app(captcha: Captcha) -> App {
    App::new().captcha(captcha).post(
        "/signup",
        |captcha: Captcha, Form(form): Form<Signup>| async move {
            captcha
                .verify(&form.captcha, Some([203, 0, 113, 7].into()))
                .await?;
            Ok::<_, Error>("welcome")
        },
    )
}

fn form(token: &str) -> String {
    format!("cf-turnstile-response={token}")
}

#[tokio::test]
async fn accepted_and_rejected_tokens() {
    let (server, seen) = provider().await;
    let url = format!("http://{}/siteverify", server.addr);
    let c = app(Captcha::turnstile("s3cr&t").verify_url(url)).test();
    let send = |token: &str| {
        c.post("/signup")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form(token))
            .send()
    };

    let ok = send("good").await;
    assert_eq!(ok.status(), 200, "{}", ok.text());
    assert_eq!(ok.text(), "welcome");
    // The secret is form-encoded, and the client's IP goes along.
    let sent = seen.lock().unwrap()[0].clone();
    assert!(sent.contains("secret=s3cr%26t"), "{sent}");
    assert!(sent.contains("response=good"), "{sent}");
    assert!(sent.contains("remoteip=203%2E0%2E113%2E7"), "{sent}");

    let bad = send("forged").await;
    assert_eq!(bad.status(), 400);
    assert!(
        bad.text().contains(r#""code":"captcha_failed""#),
        "{}",
        bad.text()
    );

    // No widget token: rejected without asking the provider.
    let before = seen.lock().unwrap().len();
    let missing = send("").await;
    assert_eq!(missing.status(), 400);
    assert!(missing.text().contains(r#""code":"captcha_missing""#));
    assert_eq!(seen.lock().unwrap().len(), before);
}

#[tokio::test]
async fn an_unreachable_or_confused_provider_fails_closed() {
    // Nothing listens on port 1.
    let down = app(Captcha::hcaptcha("s").verify_url("http://127.0.0.1:1/siteverify")).test();
    let r = down
        .post("/signup")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form("good"))
        .send()
        .await;
    assert_eq!(r.status(), 503);
    assert!(
        r.text().contains(r#""code":"captcha_unavailable""#),
        "{}",
        r.text()
    );

    // An answer without `success` is not a yes.
    let (server, _) = provider().await;
    let url = format!("http://{}/siteverify", server.addr);
    let confused = app(Captcha::turnstile("s").verify_url(url)).test();
    let r = confused
        .post("/signup")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form("broken"))
        .send()
        .await;
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn a_slow_provider_times_out() {
    let mut slow = App::new().post("/siteverify", || async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        r#"{"success":true}"#
    });
    slow.server_config_mut().banner = false;
    let server = TestServer::spawn(slow).await;
    let url = format!("http://{}/siteverify", server.addr);
    let c = app(Captcha::turnstile("s")
        .verify_url(url)
        .timeout(Duration::from_millis(200)))
    .test();
    let r = c
        .post("/signup")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form("good"))
        .send()
        .await;
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn without_a_registered_captcha_the_handler_fails_loudly() {
    let c = App::new()
        .post("/signup", |_c: Captcha| async { "never" })
        .test();
    assert_eq!(c.post("/signup").send().await.status(), 500);
}
