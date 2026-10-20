//! CSRF: (signed) double-submit cookie middleware and the `CsrfToken`
//! extractor.
#![cfg(feature = "cookies")]

use serde_json::Value;
use zapline::auth::{Csrf, CsrfToken};
use zapline::cookies::{Key, SameSite};
use zapline::prelude::*;
use zapline::test::{TestClient, TestResponse};

/// The default cookie name (the cookie is `Secure` by default).
const COOKIE: &str = "__Host-zapline.csrf";

fn app(csrf: Csrf) -> TestClient {
    routes(App::new().wrap(csrf)).test()
}

/// The same app with a registered cookie key: tokens are signed.
fn signed_app(csrf: Csrf) -> TestClient {
    routes(App::new().cookie_key(Key::from(&[7u8; 64])).wrap(csrf)).test()
}

fn routes(app: App) -> App {
    app.get("/form", |t: CsrfToken| async move { t.as_str().to_owned() })
        .get("/html", |t: CsrfToken| async move { t.hidden_input() })
        .post("/submit", || async { "accepted" })
        .put("/submit", || async { "accepted" })
        .patch("/submit", || async { "accepted" })
        .delete("/submit", || async { "accepted" })
        .post("/echo", |body: String| async move { body })
        .post("/webhooks/github", || async { "hook" })
        .post("/exact", || async { "exact" })
        .post("/exact/sub", || async { "sub" })
}

fn csrf_cookie(r: &TestResponse) -> Option<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with(&format!("{COOKIE}=")))
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
    assert!(set.starts_with(&format!("{COOKIE}={token}")), "{set}");
    // `__Host-` cookies must be Secure, on `/` and without a Domain.
    assert!(!set.contains("Domain"), "{set}");
    for attr in ["Path=/", "Secure", "SameSite=Lax"] {
        assert!(set.contains(attr), "{attr} missing in {set}");
    }
    // Readable by JavaScript (double submit).
    assert!(!set.contains("HttpOnly"));

    // A client that already has a token keeps it and gets no new cookie.
    let r = c
        .get("/form")
        .header("cookie", format!("{COOKIE}={token}"))
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
        .header("cookie", format!("{COOKIE}=short"))
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
        .header("cookie", format!("{COOKIE}={}", "a".repeat(43)))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("x".repeat(1000))
        .send()
        .await;
    assert_eq!(r.status(), 413);
}

// ---------------------------------------------------------------------------
// Planted cookies (an attacker on a sibling subdomain is same-site, so its
// cookies reach us despite `SameSite`; a more specific `Path` puts them
// first in the `Cookie` header).

/// POST /submit with this `Cookie` header and `x-csrf-token`.
async fn submit(c: &TestClient, cookie: &str, token: &str) -> u16 {
    c.post("/submit")
        .header("cookie", cookie)
        .header("x-csrf-token", token)
        .send()
        .await
        .status()
        .as_u16()
}

#[tokio::test]
async fn default_cookie_name_has_the_host_prefix() {
    let r = app(Csrf::new()).get("/form").send().await;
    let set = r.header("set-cookie").unwrap();
    assert!(set.starts_with("__Host-zapline.csrf="), "{set}");
    for attr in ["Secure", "Path=/"] {
        assert!(set.contains(attr), "{attr} missing in {set}");
    }
    assert!(!set.contains("Domain"), "{set}");

    // Without `Secure` (plain-HTTP development) browsers would drop a
    // `__Host-` cookie, so the prefix goes too.
    let c = app(Csrf::new().secure(false));
    let r = c.get("/form").send().await;
    let set = r.header("set-cookie").unwrap().to_owned();
    assert!(set.starts_with("zapline.csrf="), "{set}");
    assert!(!set.contains("Secure"), "{set}");
    let token = r.text();
    let cookie = format!("zapline.csrf={token}");
    assert_eq!(submit(&c, &cookie, &token).await, 200);
    let prefixed = format!("{COOKIE}={token}");
    assert_eq!(submit(&c, &prefixed, &token).await, 403);

    // An explicit name wins either way.
    for csrf in [Csrf::new(), Csrf::new().secure(false)] {
        let r = app(csrf.cookie_name("xsrf")).get("/form").send().await;
        let set = r.header("set-cookie").unwrap();
        assert!(set.starts_with("xsrf="), "{set}");
    }
}

#[tokio::test]
async fn signed_tokens_reject_planted_values() {
    let c = signed_app(Csrf::new());
    let (token, cookie) = token(&c).await;
    // random || HMAC-SHA256, base64url.
    assert_eq!(token.len(), 86);
    assert!(token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));

    // A token this server minted is accepted (header and form field).
    assert_eq!(submit(&c, &cookie, &token).await, 200);
    let r = c
        .post("/echo")
        .header("cookie", cookie.clone())
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("csrf_token={token}"))
        .send()
        .await;
    assert_eq!(r.status(), 200);

    // The attacker plants a cookie with a value of its choosing and submits
    // the same value: unsigned, forged or signed with another key.
    let unsigned = "A".repeat(43);
    let mut forged = token[..43].to_owned();
    forged.push_str(&"A".repeat(43));
    let mut tampered = token.clone().into_bytes();
    tampered[60] = if tampered[60] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).unwrap();
    let other_key = signed_token(Csrf::new().key(Key::from(&[8u8; 64]))).await;
    for planted in [&unsigned, &forged, &tampered, &other_key] {
        let cookie = format!("{COOKIE}={planted}");
        assert_eq!(submit(&c, &cookie, planted).await, 403, "{planted}");
        // ...also listed ahead of the victim's real cookie.
        let both = format!("{COOKIE}={planted}; {COOKIE}={token}");
        assert_eq!(submit(&c, &both, planted).await, 403, "{planted}");
        // The planted cookie is ignored: the victim's own token still works.
        assert_eq!(submit(&c, &both, &token).await, 200, "{planted}");
    }

    // A client holding only an invalid cookie gets a fresh signed token.
    let r = c
        .get("/form")
        .header("cookie", format!("{COOKIE}={unsigned}"))
        .send()
        .await;
    let fresh = r.text();
    assert_eq!(fresh.len(), 86);
    assert!(csrf_cookie(&r)
        .unwrap()
        .starts_with(&format!("{COOKIE}={fresh}")));
}

/// A token minted by a separate app using `csrf`.
async fn signed_token(csrf: Csrf) -> String {
    let (token, _) = token(&app(csrf)).await;
    token
}

#[tokio::test]
async fn layer_key_signs_without_an_app_key() {
    let key = Key::from(&[9u8; 64]);
    let c = app(Csrf::new().key(key.clone()));
    let (token, cookie) = token(&c).await;
    assert_eq!(token.len(), 86);
    assert_eq!(submit(&c, &cookie, &token).await, 200);

    // Unsigned tokens are refused once a key is in play...
    let unsigned = "A".repeat(43);
    let planted = format!("{COOKIE}={unsigned}");
    assert_eq!(submit(&c, &planted, &unsigned).await, 403);
    // ...and the layer's key takes precedence over the app's.
    let c = routes(
        App::new()
            .cookie_key(Key::from(&[1u8; 64]))
            .wrap(Csrf::new().key(key)),
    )
    .test();
    assert_eq!(submit(&c, &cookie, &token).await, 200);
}

#[tokio::test]
async fn several_token_cookies_must_agree() {
    // Unsigned tokens: any well-formed value could have been planted, so
    // two different ones fail the request whichever is submitted.
    let c = app(Csrf::new());
    let (token, cookie) = token(&c).await;
    let planted = "A".repeat(43);
    let both = format!("{COOKIE}={planted}; {cookie}");
    assert_eq!(submit(&c, &both, &planted).await, 403);
    assert_eq!(submit(&c, &both, &token).await, 403);
    // Also when split over several `Cookie` headers (HTTP/2).
    let r = c
        .post("/submit")
        .header("cookie", format!("{COOKIE}={planted}"))
        .header("cookie", cookie.clone())
        .header("x-csrf-token", token.clone())
        .send()
        .await;
    assert_eq!(r.status(), 403);
    // The same token twice (e.g. set on two paths) is fine, and malformed
    // values never count.
    let twice = format!("{cookie}; {cookie}");
    assert_eq!(submit(&c, &twice, &token).await, 200);
    let junk = format!("{COOKIE}=short; {cookie}");
    assert_eq!(submit(&c, &junk, &token).await, 200);

    // Signed tokens: a token the server minted for the attacker is valid,
    // but it can't be told apart from the victim's, so neither is used.
    let c = signed_app(Csrf::new());
    let (victim, cookie) = self::token(&c).await;
    let (attacker, _) = self::token(&c).await;
    let both = format!("{COOKIE}={attacker}; {cookie}");
    assert_eq!(submit(&c, &both, &attacker).await, 403);
    assert_eq!(submit(&c, &both, &victim).await, 403);
}
