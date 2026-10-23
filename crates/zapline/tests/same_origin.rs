//! `SameOrigin`: cross-site browser requests can't change state; same-site
//! pages, non-browser clients and safe methods pass.
#![cfg(feature = "json")]

use zapline::middleware::SameOrigin;
use zapline::prelude::*;
use zapline::test::TestClient;

fn client(guard: SameOrigin) -> TestClient {
    App::new()
        .wrap(guard)
        .get("/todos", || async { "list" })
        .post("/todos", || async { "created" })
        .delete("/todos/1", || async { "deleted" })
        .post("/webhooks/stripe", || async { "ok" })
        .test()
}

async fn post(c: &TestClient, headers: &[(&str, &str)]) -> u16 {
    let mut req = c.post("/todos").header("host", "shop.example");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().await.status().as_u16()
}

#[tokio::test]
async fn cross_site_writes_are_refused() {
    let c = client(SameOrigin::new());
    // Browsers with Fetch Metadata.
    assert_eq!(post(&c, &[("sec-fetch-site", "same-origin")]).await, 200);
    assert_eq!(post(&c, &[("sec-fetch-site", "none")]).await, 200);
    assert_eq!(post(&c, &[("sec-fetch-site", "cross-site")]).await, 403);
    // A sibling subdomain: SameSite=Lax cookies would still be sent.
    assert_eq!(post(&c, &[("sec-fetch-site", "same-site")]).await, 403);
    // Older browsers: Origin against Host.
    assert_eq!(post(&c, &[("origin", "https://shop.example")]).await, 200);
    assert_eq!(post(&c, &[("origin", "https://evil.example")]).await, 403);
    assert_eq!(post(&c, &[("origin", "null")]).await, 403);
    // Not a browser (curl, API clients, tests).
    assert_eq!(post(&c, &[]).await, 200);

    let res = c
        .delete("/todos/1")
        .header("sec-fetch-site", "cross-site")
        .send()
        .await;
    res.assert_status(403);
    assert_eq!(res.json::<serde_json::Value>()["code"], "cross_origin");
    // Reads always pass.
    c.get("/todos")
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn allowed_origins_and_exempt_paths() {
    let c = client(
        SameOrigin::new()
            .allow(["https://app.example/"])
            .exempt(["/webhooks/"]),
    );
    let from_app = [
        ("sec-fetch-site", "same-site"),
        ("origin", "https://app.example"),
    ];
    assert_eq!(post(&c, &from_app).await, 200);
    let from_evil = [
        ("sec-fetch-site", "cross-site"),
        ("origin", "https://evil.example"),
    ];
    assert_eq!(post(&c, &from_evil).await, 403);
    c.post("/webhooks/stripe")
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .assert_status(200);
}

#[test]
fn trusted_proxies_from_the_environment() {
    use zapline::middleware::rate_limit::TrustedProxies;
    // One test touches the variable, so parallel tests don't race on it.
    std::env::set_var("TRUSTED_PROXIES", " 10.0.0.0/8, fd00::/8 ,");
    let proxies = TrustedProxies::from_env().unwrap();
    assert!(proxies.is_trusted("10.1.2.3".parse().unwrap()));
    assert!(proxies.is_trusted("fd00::1".parse().unwrap()));
    assert!(!proxies.is_trusted("203.0.113.7".parse().unwrap()));
    std::env::set_var("TRUSTED_PROXIES", "10.0.0.0/8,nope");
    let err = TrustedProxies::from_env().unwrap_err();
    assert!(err.message().contains("\"nope\""), "{}", err.message());
    std::env::remove_var("TRUSTED_PROXIES");
    assert!(!TrustedProxies::from_env()
        .unwrap()
        .is_trusted("10.1.2.3".parse().unwrap()));
}
