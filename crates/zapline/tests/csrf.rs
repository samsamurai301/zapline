//! CSRF: double-submit cookie middleware and the `CsrfToken` extractor.
#![cfg(feature = "cookies")]

use serde_json::Value;
use zapline::auth::{Csrf, CsrfToken};
use zapline::cookies::SameSite;
use zapline::prelude::*;
use zapline::test::{TestClient, TestResponse};

fn app(csrf: Csrf) -> TestClient {
    App::new()
        .wrap(csrf)
        .get("/form", |t: CsrfToken| async move { t.as_str().to_owned() })
        .get("/html", |t: CsrfToken| async move { t.hidden_input() })
        .post("/submit", || async { "accepted" })
        .put("/submit", || async { "accepted" })
        .patch("/submit", || async { "accepted" })
        .delete("/submit", || async { "accepted" })
        .post("/echo", |body: String| async move { body })
        .post("/webhooks/github", || async { "hook" })
        .post("/exact", || async { "exact" })
        .post("/exact/sub", || async { "sub" })
        .test()
}

fn csrf_cookie(r: &TestResponse) -> Option<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("zapline.csrf="))
        .map(str::to_owned)
}

/// GET a page and return (token, `Cookie` header value).
async fn token(c: &TestClient) -> (String, String) {
    let r = c.get("/form").send().await;
    let set = csrf_cookie(&r).expect("token cookie");
    let pair = set.split(';').next().unwrap().to_owned();
    (r.text(), pair)
}

fn code(r: &TestResponse) -> String {
    r.json::<Value>()["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn safe_requests_get_a_token_cookie() {
    let c = app(Csrf::new());
    let r = c.get("/form").send().await;
    let set = csrf_cookie(&r).unwrap();
    let token = r.text();
    assert_eq!(token.len(), 43);
    assert!(set.starts_with(&format!("zapline.csrf={token}")), "{set}");
    for attr in ["Path=/", "Secure", "SameSite=Lax"] {
        assert!(set.contains(attr), "{attr} missing in {set}");
    }
    // Readable by JavaScript (double submit).
    assert!(!set.contains("HttpOnly"));

    // A client that already has a token keeps it and gets no new cookie.
    let r = c
        .get("/form")
        .header("cookie", format!("zapline.csrf={token}"))
        .send()
        .await;
    assert_eq!(r.text(), token);
    assert!(csrf_cookie(&r).is_none());

    // Each client gets its own random token.
    let other = c.get("/form").send().await.text();
    assert_ne!(other, token);
}

#[tokio::test]
async fn unsafe_methods_need_a_matching_token() {
    let c = app(Csrf::new());
    let (token, cookie) = token(&c).await;

    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        // Header matches cookie.
        let r = c
            .request(method.clone(), "/submit")
            .header("cookie", cookie.clone())
            .header("x-csrf-token", token.clone())
            .send()
            .await;
        assert_eq!(r.text(), "accepted", "{method}");

        // No header.
        let r = c
            .request(method.clone(), "/submit")
            .header("cookie", cookie.clone())
            .send()
            .await;
        assert_eq!(
            (r.status().as_u16(), code(&r)),
            (403, "csrf_failed".into()),
            "{method}"
        );
    }

    // Wrong token.
    let (other, _) = self::token(&c).await;
    let r = c
        .post("/submit")
        .header("cookie", cookie.clone())
        .header("x-csrf-token", other.clone())
        .send()
        .await;
    assert_eq!(r.status(), 403);

    // Header without cookie (the attacker's page cannot read the cookie).
    let r = c
        .post("/submit")
        .header("x-csrf-token", token.clone())
        .send()
        .await;
    assert_eq!(r.status(), 403);

    // A planted cookie with a bogus format is ignored.
    let r = c
        .post("/submit")
        .header("cookie", "zapline.csrf=short")
        .header("x-csrf-token", "short")
        .send()
        .await;
    assert_eq!(r.status(), 403);

    // Duplicate headers are rejected.
    let r = c
        .post("/submit")
        .header("cookie", cookie.clone())
        .header("x-csrf-token", token.clone())
        .header("x-csrf-token", token.clone())
        .send()
        .await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn form_field_is_accepted_and_body_preserved() {
    let c = app(Csrf::new());
    let (token, cookie) = token(&c).await;
    let body = format!("name=ada+lovelace&csrf_token={token}&x=1");
    let r = c
        .post("/echo")
        .header("cookie", cookie.clone())
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.clone())
        .send()
        .await;
    assert_eq!(r.status(), 200);
    // The handler still sees the full body.
    assert_eq!(r.text(), body);

    let r = c
        .post("/echo")
        .header("cookie", cookie.clone())
        .header("content-type", "application/x-www-form-urlencoded")
        .body("name=x&csrf_token=wrong")
        .send()
        .await;
    assert_eq!(r.status(), 403);

    // Only urlencoded bodies are inspected.
    let r = c
        .post("/echo")
        .header("cookie", cookie)
        .header("content-type", "text/plain")
        .body(format!("csrf_token={token}"))
        .send()
        .await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn exempt_paths() {
    let c = app(Csrf::new().exempt("/webhooks/*").exempt("/exact"));
    assert_eq!(c.post("/webhooks/github").send().await.text(), "hook");
    assert_eq!(c.post("/exact").send().await.text(), "exact");
    assert_eq!(c.post("/exact/sub").send().await.status(), 403);
    assert_eq!(c.post("/submit").send().await.status(), 403);
}

#[tokio::test]
async fn custom_names_and_attributes() {
    let csrf = Csrf::new()
        .cookie_name("__Host-csrf")
        .header_name("x-xsrf")
        .form_field("_token")
        .secure(false)
        .same_site(SameSite::Strict);
    let c = App::new()
        .wrap(csrf)
        .get("/", |t: CsrfToken| async move {
            format!("{}|{}", t.field_name(), t)
        })
        .post("/", || async { "ok" })
        .test();
    let r = c.get("/").send().await;
    let set = r.header("set-cookie").unwrap().to_owned();
    assert!(set.starts_with("__Host-csrf="), "{set}");
    assert!(
        set.contains("SameSite=Strict") && !set.contains("Secure"),
        "{set}"
    );
    let (field, token) = r
        .text()
        .split_once('|')
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .unwrap();
    assert_eq!(field, "_token");
    let cookie = set.split(';').next().unwrap().to_owned();

    let r = c
        .post("/")
        .header("cookie", cookie.clone())
        .header("x-xsrf", token.clone())
        .send()
        .await;
    assert_eq!(r.text(), "ok");
    let r = c
        .post("/")
        .header("cookie", cookie.clone())
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_token={token}"))
        .send()
        .await;
    assert_eq!(r.text(), "ok");
    // The default header name is no longer accepted.
    let r = c
        .post("/")
        .header("cookie", cookie)
        .header("x-csrf-token", token)
        .send()
        .await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn token_rendering_and_missing_middleware() {
    let c = app(Csrf::new());
    let r = c.get("/html").send().await;
    let html = r.text();
    assert!(
        html.starts_with(r#"<input type="hidden" name="csrf_token" value=""#),
        "{html}"
    );

    let plain = App::new()
        .get("/", |t: CsrfToken| async move { t.to_string() })
        .test();
    let r = plain.get("/").send().await;
    assert_eq!(
        (r.status().as_u16(), code(&r)),
        (500, "csrf_layer_missing".into())
    );
}

#[tokio::test]
async fn oversized_form_bodies_are_refused() {
    let c = App::new()
        .body_limit(64)
        .wrap(Csrf::new())
        .post("/", || async { "ok" })
        .test();
    let r = c
        .post("/")
        .header("cookie", format!("zapline.csrf={}", "a".repeat(43)))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("x".repeat(1000))
        .send()
        .await;
    assert_eq!(r.status(), 413);
}
