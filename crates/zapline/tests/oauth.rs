//! OAuth logins against an in-process fake provider on a real socket.
#![cfg(all(feature = "oauth", feature = "macros"))]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zapline::auth::oauth::{OAuth, OAuthLogin};
use zapline::prelude::*;

/// What the fake provider remembers: code -> (PKCE challenge, redirect_uri).
#[derive(Clone, Default)]
struct Issued(Arc<Mutex<HashMap<String, (String, String)>>>);

fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| {
            let d = |s: &str| {
                percent_encoding::percent_decode_str(&s.replace('+', " "))
                    .decode_utf8_lossy()
                    .into_owned()
            };
            (d(k), d(v))
        })
        .collect()
}

async fn token(State(issued): State<Issued>, body: String) -> Response {
    let f = form(&body);
    let ok = f.get("grant_type").map(String::as_str) == Some("authorization_code")
        && f.get("client_id").map(String::as_str) == Some("app-id")
        && f.get("client_secret").map(String::as_str) == Some("app-secret");
    let known = f
        .get("code")
        .and_then(|c| issued.0.lock().unwrap().remove(c));
    match (ok, known) {
        (true, Some((challenge, redirect_uri)))
            if f.get("redirect_uri") == Some(&redirect_uri)
                && f.get("code_verifier")
                    .is_some_and(|v| b64url(&Sha256::digest(v.as_bytes())) == challenge) =>
        {
            Json(json!({ "access_token": "tok-123", "token_type": "bearer" })).into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant", "error_description": "bad code" })),
        )
            .into_response(),
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer tok-123")
}

async fn me(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({
        "sub": "u-42", "email": "ada@example.com", "email_verified": true,
        "name": "Ada", "picture": "https://img/ada.png"
    }))
    .into_response()
}

async fn gh_user(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(
        json!({ "id": 583231, "login": "octocat", "email": null, "avatar_url": "https://a/o.png" }),
    )
    .into_response()
}

async fn gh_emails(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!([
        { "email": "old@example.com", "primary": false, "verified": true },
        { "email": "octo@example.com", "primary": true, "verified": true }
    ]))
    .into_response()
}

async fn provider(issued: Issued) -> SocketAddr {
    let app = App::new()
        .state(issued)
        .post("/token", token)
        .get("/me", me)
        .get("/user", gh_user)
        .get("/user/emails", gh_emails);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    addr
}

#[get("/auth/fake/callback")]
async fn fake_done(login: OAuthLogin) -> Json<Value> {
    Json(json!({ "user": login.user, "token": login.access_token, "next": login.next }))
}

#[get("/auth/github/callback")]
async fn github_done(login: OAuthLogin) -> Json<Value> {
    Json(json!({ "user": login.user }))
}

fn query(url: &str) -> HashMap<String, String> {
    form(url.split_once('?').map_or("", |(_, q)| q))
}

/// Start a login, play the provider's part (issue a code for the PKCE
/// challenge), and return the callback query plus the state cookie.
async fn begin(
    client: &zapline::test::TestClient,
    issued: &Issued,
    start: &str,
) -> (String, String) {
    let res = client.get(start).send().await;
    res.assert_status(303);
    let location = res.header("location").unwrap().to_owned();
    let set_cookie = res.header("set-cookie").unwrap().to_owned();
    assert!(
        set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Lax"),
        "{set_cookie}"
    );
    let q = query(&location);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    issued.0.lock().unwrap().insert(
        "code-1".into(),
        (q["code_challenge"].clone(), q["redirect_uri"].clone()),
    );
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    (format!("code=code-1&state={}", q["state"]), cookie)
}

#[tokio::test]
async fn a_full_login_with_pkce() {
    let issued = Issued::default();
    let addr = provider(issued.clone()).await;
    let fake = OAuth::custom(
        "fake",
        format!("http://{addr}/authorize"),
        format!("http://{addr}/token"),
        format!("http://{addr}/me"),
    )
    .credentials("app-id", "app-secret")
    .scopes(["openid", "email"]);
    let client = App::new().oauth(fake).mount(fake_done).test();

    let res = client.get("/auth/fake?next=/settings").send().await;
    let location = res.header("location").unwrap();
    assert!(
        location.starts_with(&format!("http://{addr}/authorize?")),
        "{location}"
    );
    let q = query(location);
    assert_eq!(q["client_id"], "app-id");
    assert_eq!(q["scope"], "openid email");
    assert!(
        q["redirect_uri"].ends_with("/auth/fake/callback"),
        "{}",
        q["redirect_uri"]
    );

    let (callback, cookie) = begin(&client, &issued, "/auth/fake?next=/settings").await;
    let res = client
        .get(&format!("/auth/fake/callback?{callback}"))
        .header("cookie", cookie.as_str())
        .send()
        .await;
    res.assert_status(200);
    let body: Value = res.json();
    assert_eq!(body["token"], "tok-123");
    assert_eq!(body["next"], "/settings");
    let user = &body["user"];
    assert_eq!(user["provider"], "fake");
    assert_eq!(user["id"], "u-42");
    assert_eq!(user["email"], "ada@example.com");
    assert_eq!(user["email_verified"], true);
    assert_eq!(user["name"], "Ada");
    assert_eq!(user["avatar_url"], "https://img/ada.png");

    // The code was single-use at the provider: replaying it fails.
    let res = client
        .get(&format!("/auth/fake/callback?{callback}"))
        .header("cookie", cookie.as_str())
        .send()
        .await;
    res.assert_status(401);
    assert_eq!(res.json::<Value>()["code"], "oauth_failed");
}

#[tokio::test]
async fn forged_or_cancelled_logins_are_refused() {
    let issued = Issued::default();
    let addr = provider(issued.clone()).await;
    let fake = OAuth::custom(
        "fake",
        format!("http://{addr}/authorize"),
        format!("http://{addr}/token"),
        format!("http://{addr}/me"),
    )
    .credentials("app-id", "app-secret");
    let client = App::new().oauth(fake).mount(fake_done).test();
    let (callback, cookie) = begin(&client, &issued, "/auth/fake?next=//evil.com").await;

    // No cookie (started elsewhere), or a state that doesn't match it.
    let res = client
        .get(&format!("/auth/fake/callback?{callback}"))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json::<Value>()["code"], "invalid_oauth_state");
    let res = client
        .get("/auth/fake/callback?code=code-1&state=forged")
        .header("cookie", cookie.as_str())
        .send()
        .await;
    res.assert_status(400);

    // The user said no at the provider.
    let res = client
        .get("/auth/fake/callback?error=access_denied&error_description=User+declined")
        .send()
        .await;
    res.assert_status(401);
    assert!(res.text().contains("User declined"), "{}", res.text());

    // An outside `next` is dropped.
    let res = client
        .get(&format!("/auth/fake/callback?{callback}"))
        .header("cookie", cookie.as_str())
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json::<Value>()["next"], Value::Null);
}

#[tokio::test]
async fn github_uses_the_verified_primary_email() {
    let issued = Issued::default();
    let addr = provider(issued.clone()).await;
    let github = OAuth::github("app-id", "app-secret").endpoints(
        format!("http://{addr}/authorize"),
        format!("http://{addr}/token"),
        format!("http://{addr}/user"),
    );
    let client = App::new().oauth(github).mount(github_done).test();
    let (callback, cookie) = begin(&client, &issued, "/auth/github").await;
    let res = client
        .get(&format!("/auth/github/callback?{callback}"))
        .header("cookie", cookie.as_str())
        .send()
        .await;
    res.assert_status(200);
    let user = &res.json::<Value>()["user"];
    assert_eq!(user["id"], "583231");
    assert_eq!(user["email"], "octo@example.com");
    assert_eq!(user["email_verified"], true);
    assert_eq!(user["name"], "octocat");
}

#[tokio::test]
async fn unknown_providers_are_404() {
    let client = App::new().oauth(OAuth::github("a", "b")).test();
    client.get("/auth/gitlab").send().await.assert_status(404);
    client.get("/auth/github").send().await.assert_status(303);
}
