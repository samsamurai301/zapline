//! The test client's cookie store behaves like a browser's: `Domain`,
//! `Path`, `Secure`, `SameSite`, expiry, `HttpOnly` and cookie prefixes,
//! plus origins (`base_url`, `from_origin`, `fetch_from`) and the fetch
//! metadata headers they add.

use std::time::{Duration, SystemTime};

use zapline::prelude::*;
use zapline::test::{CookieSameSite, TestClient};

/// Every path echoes the `Cookie` header it got and answers with one
/// `Set-Cookie` per `x-set` request header.
fn app() -> App {
    async fn echo(headers: HeaderMap) -> Response {
        let cookies = headers
            .get(header::COOKIE)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let mut res = cookies.into_response();
        for v in headers.get_all("x-set") {
            res.headers_mut().append(header::SET_COOKIE, v.clone());
        }
        res
    }
    App::new()
        .get("/", echo)
        .get("/{*rest}", echo)
        .post("/", echo)
        .post("/{*rest}", echo)
}

fn browser(base: &str) -> TestClient {
    app().test().with_cookies().base_url(base)
}

async fn set(b: &TestClient, url: &str, cookies: &[&str]) {
    let mut req = b.get(url);
    for c in cookies {
        req = req.header("x-set", *c);
    }
    req.send().await.assert_status(200);
}

async fn sent(b: &TestClient, url: &str) -> String {
    b.get(url).send().await.text()
}

#[tokio::test]
async fn domain_scopes_cookies() {
    let b = browser("https://app.example.com");
    set(
        &b,
        "/set",
        &[
            "host=1",
            "parent=1; Domain=example.com",
            "dotted=1; Domain=.EXAMPLE.com",
            "foreign=1; Domain=other.com",
            "suffix=1; Domain=com",
            "uk=1; Domain=co.uk",
            "child=1; Domain=api.app.example.com",
        ],
    )
    .await;
    let names: Vec<String> = b.cookies().into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["host", "parent", "dotted"]);
    let parent = b
        .cookies()
        .into_iter()
        .find(|c| c.name == "parent")
        .unwrap();
    assert_eq!(parent.domain, "example.com");
    assert!(!parent.host_only);
    let host = b.cookies().into_iter().find(|c| c.name == "host").unwrap();
    assert_eq!(host.domain, "app.example.com");
    assert!(host.host_only);

    assert_eq!(sent(&b, "/").await, "host=1; parent=1; dotted=1");
    // Host-only cookies stay on their host; domain cookies cover subdomains
    // and the domain itself.
    assert_eq!(
        sent(&b, "https://api.app.example.com/").await,
        "parent=1; dotted=1"
    );
    assert_eq!(sent(&b, "https://example.com/").await, "parent=1; dotted=1");
    assert_eq!(sent(&b, "https://notexample.com/").await, "");
    assert_eq!(sent(&b, "https://other.com/").await, "");
}

#[tokio::test]
async fn ip_hosts_take_no_domain_cookies() {
    let b = browser("http://127.0.0.1:3000");
    set(
        &b,
        "/set",
        &["a=1", "b=1; Domain=0.0.1", "c=1; Domain=127.0.0.1"],
    )
    .await;
    assert_eq!(sent(&b, "/").await, "a=1; c=1");
}

#[tokio::test]
async fn paths_scope_cookies_and_order_them() {
    let b = browser("https://app.example.com");
    set(
        &b,
        "/set/account/login",
        &["dir=1", "root=1; Path=/", "deep=1; Path=/account/settings"],
    )
    .await;
    // `dir` defaults to the request's directory, `/set/account`.
    assert_eq!(sent(&b, "/set/account/x").await, "dir=1; root=1");
    assert_eq!(sent(&b, "/account/settings/2fa").await, "deep=1; root=1");
    assert_eq!(sent(&b, "/account").await, "root=1");
    // Same name on two paths: both are sent, the longer path first.
    set(&b, "/set", &["root=2; Path=/account"]).await;
    assert_eq!(
        sent(&b, "/account/settings").await,
        "deep=1; root=2; root=1"
    );
}

#[tokio::test]
async fn secure_cookies_need_a_secure_request() {
    let b = browser("http://app.example.com");
    set(
        &b,
        "/set",
        &["plain=1", "sec=1; Secure", "__Secure-a=1; Secure"],
    )
    .await;
    // Rejected over plain http.
    assert_eq!(sent(&b, "/").await, "plain=1");

    set(&b, "https://app.example.com/set", &["sec=1; Secure"]).await;
    assert_eq!(sent(&b, "https://app.example.com/").await, "plain=1; sec=1");
    // Not sent over http...
    assert_eq!(sent(&b, "/").await, "plain=1");
    // ...and http can't overwrite or shadow it.
    set(&b, "/set", &["sec=evil", "sec=evil; Path=/deeper"]).await;
    assert_eq!(
        sent(&b, "https://app.example.com/deeper").await,
        "plain=1; sec=1"
    );

    // Loopback counts as secure, as in browsers.
    let local = app().test().with_cookies();
    set(&local, "/set", &["sec=1; Secure"]).await;
    assert_eq!(sent(&local, "/").await, "sec=1");
}

#[tokio::test]
async fn cookie_prefixes() {
    let b = browser("https://app.example.com");
    set(
        &b,
        "/set/x",
        &[
            "__Secure-ok=1; Secure",
            "__Secure-no=1",
            "__Host-ok=1; Secure; Path=/",
            "__Host-nopath=1; Secure",
            "__Host-domain=1; Secure; Path=/; Domain=app.example.com",
            "__host-lower=1; Path=/",
        ],
    )
    .await;
    let names: Vec<String> = b.cookies().into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["__Secure-ok", "__Host-ok"]);
}

#[tokio::test]
async fn same_site_on_cross_site_requests() {
    let b = browser("https://app.example.com");
    set(
        &b,
        "/set",
        &[
            "strict=1; SameSite=Strict",
            "lax=1; SameSite=Lax",
            "default=1",
            "none=1; SameSite=None; Secure",
            "insecure_none=1; SameSite=None",
        ],
    )
    .await;
    let all = "strict=1; lax=1; default=1; none=1";
    assert_eq!(sent(&b, "/").await, all);
    // A sibling subdomain is the same site: everything is sent.
    let r = b
        .post("/")
        .from_origin("https://blog.example.com")
        .send()
        .await;
    assert_eq!(r.text(), all);
    // A link from another site (top-level GET): Lax and None.
    let r = b.get("/").from_origin("https://evil.example").send().await;
    assert_eq!(r.text(), "lax=1; default=1; none=1");
    // A form posted from another site: only None.
    let r = b.post("/").from_origin("https://evil.example").send().await;
    assert_eq!(r.text(), "none=1");
    // A cross-site fetch, even a GET: only None.
    let r = b.get("/").fetch_from("https://evil.example").send().await;
    assert_eq!(r.text(), "none=1");
    // Schemeful: http://app.example.com is another site.
    let r = b
        .post("/")
        .from_origin("http://app.example.com")
        .send()
        .await;
    assert_eq!(r.text(), "none=1");

    let c = b.cookies();
    let attr = |n: &str| c.iter().find(|c| c.name == n).unwrap().same_site;
    assert_eq!(attr("strict"), Some(CookieSameSite::Strict));
    assert_eq!(attr("lax"), Some(CookieSameSite::Lax));
    assert_eq!(attr("none"), Some(CookieSameSite::None));
    assert_eq!(attr("default"), None);
}

#[tokio::test]
async fn cross_site_responses_set_only_what_browsers_allow() {
    let b = browser("https://app.example.com");
    let cookies = [
        "lax=1",
        "strict=1; SameSite=Strict",
        "none=1; SameSite=None; Secure",
    ];
    // A cross-site fetch can only set SameSite=None cookies.
    let mut req = b.get("/set").fetch_from("https://evil.example");
    for c in cookies {
        req = req.header("x-set", c);
    }
    req.send().await;
    assert_eq!(sent(&b, "/").await, "none=1");
    // A cross-site top-level navigation can set any of them.
    let mut req = b.get("/set").from_origin("https://evil.example");
    for c in cookies {
        req = req.header("x-set", c);
    }
    req.send().await;
    assert_eq!(sent(&b, "/").await, "none=1; lax=1; strict=1");
}

#[tokio::test]
async fn expiry() {
    let b = browser("https://app.example.com");
    let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(60));
    let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(3600));
    set(
        &b,
        "/set",
        &[
            "session=1",
            &format!("dated=1; Expires={future}"),
            &format!("gone=1; Expires={past}"),
            &format!("wins=1; Max-Age=60; Expires={past}"),
            "short=1; Max-Age=1",
            "zero=1; Max-Age=0",
            "bad=1; Max-Age=soon",
        ],
    )
    .await;
    assert_eq!(
        sent(&b, "/").await,
        "session=1; dated=1; wins=1; short=1; bad=1"
    );
    let c = b.cookies();
    let expires = |n: &str| c.iter().find(|c| c.name == n).unwrap().expires;
    assert!(expires("session").is_none());
    assert!(expires("dated").is_some());
    assert!(expires("bad").is_none(), "a malformed Max-Age is ignored");

    // Removing a cookie needs the same domain and path.
    set(&b, "/set", &["dated=; Max-Age=0; Path=/elsewhere"]).await;
    assert!(sent(&b, "/").await.contains("dated=1"));
    set(&b, "/set", &[&format!("dated=; Expires={past}")]).await;
    assert!(!sent(&b, "/").await.contains("dated"));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(sent(&b, "/").await, "session=1; wins=1; bad=1");
    assert_eq!(b.cookie("short"), None);
}

#[tokio::test]
async fn http_only_hides_cookies_from_scripts() {
    let b = browser("https://app.example.com");
    set(
        &b,
        "/set",
        &[
            "sid=s; HttpOnly; Secure",
            "csrf=t; Secure",
            "scoped=1; Path=/admin",
        ],
    )
    .await;
    assert_eq!(b.document_cookie("https://app.example.com/"), "csrf=t");
    assert_eq!(
        b.document_cookie("https://app.example.com/admin/users"),
        "scoped=1; csrf=t"
    );
    // Secure cookies are invisible to an insecure page.
    assert_eq!(b.document_cookie("http://app.example.com/"), "");
    // ...but the server still gets the HttpOnly one.
    assert!(sent(&b, "/").await.contains("sid=s"));
    let sid = b.cookies().into_iter().find(|c| c.name == "sid").unwrap();
    assert!(sid.http_only && sid.secure);
}

#[tokio::test]
async fn set_cookie_simulates_other_sites() {
    let b = browser("https://app.example.com");
    // A sibling subdomain can set a cookie for the parent domain...
    assert!(b.set_cookie(
        "https://evil.example.com/",
        "planted=1; Domain=example.com; Path=/"
    ));
    assert_eq!(sent(&b, "/").await, "planted=1");
    // ...but not one only app.example.com would get, nor a `__Host-` one.
    assert!(!b.set_cookie("https://evil.example.com/", "x=1; Domain=app.example.com"));
    assert!(!b.set_cookie(
        "https://evil.example.com/",
        "__Host-x=1; Domain=example.com; Path=/; Secure"
    ));
    // A host-only cookie of another host never reaches us.
    assert!(b.set_cookie("https://evil.example.com/", "mine=1"));
    assert_eq!(sent(&b, "/").await, "planted=1");

    b.clear_cookies();
    assert!(b.cookies().is_empty());
    assert_eq!(sent(&b, "/").await, "");
}

#[tokio::test]
async fn explicit_cookie_headers_win() {
    let b = browser("https://app.example.com");
    set(&b, "/set", &["a=stored", "b=stored"]).await;
    let r = b.get("/").header("cookie", "a=explicit").send().await;
    assert_eq!(r.text(), "a=explicit; b=stored");
}

#[tokio::test]
async fn base_url_and_fetch_metadata() {
    let app = App::new().post("/", |conn: ConnInfo, h: HeaderMap| async move {
        let get = |n: &str| {
            h.get(n)
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_else(|| "-".into())
        };
        format!(
            "{} host={} site={} mode={} origin={}",
            conn.secure,
            get("host"),
            get("sec-fetch-site"),
            get("sec-fetch-mode"),
            get("origin")
        )
    });
    let c = app.test().base_url("https://app.example.com:8443");
    let r = c.post("/").send().await;
    assert_eq!(
        r.text(),
        "true host=app.example.com:8443 site=- mode=- origin=-"
    );
    let r = c
        .post("/")
        .from_origin("https://app.example.com:8443")
        .send()
        .await;
    assert_eq!(
        r.text(),
        "true host=app.example.com:8443 site=same-origin mode=navigate origin=https://app.example.com:8443"
    );
    let r = c
        .post("/")
        .from_origin("https://www.example.com")
        .send()
        .await;
    assert!(r
        .text()
        .ends_with("site=same-site mode=navigate origin=https://www.example.com"));
    let r = c.post("/").fetch_from("http://evil.example").send().await;
    assert!(r
        .text()
        .ends_with("site=cross-site mode=cors origin=http://evil.example"));
    // Explicit headers win.
    let r = c
        .post("/")
        .from_origin("https://evil.example")
        .header("sec-fetch-site", "none")
        .send()
        .await;
    assert!(r.text().contains("site=none"));
    // Without a base URL nothing is added and requests aren't secure.
    let plain = App::new()
        .get("/", |conn: ConnInfo, h: HeaderMap| async move {
            format!("{} {}", conn.secure, h.contains_key("host"))
        })
        .test();
    assert_eq!(plain.get("/").send().await.text(), "false false");
    assert_eq!(
        plain.get("https://x.example/").send().await.text(),
        "true false"
    );
}

#[tokio::test]
async fn host_header_picks_the_cookie_host() {
    let b = app().test().with_cookies();
    b.get("/set")
        .header("host", "tenant.example.com")
        .header("x-set", "t=1")
        .send()
        .await;
    let t = &b.cookies()[0];
    assert_eq!(t.domain, "tenant.example.com");
    let r = b.get("/").header("host", "tenant.example.com").send().await;
    assert_eq!(r.text(), "t=1");
    assert_eq!(sent(&b, "/").await, "");
}

#[tokio::test]
async fn oversized_cookies_are_rejected() {
    let b = browser("https://app.example.com");
    let big = format!("big={}", "x".repeat(4096));
    set(&b, "/set", &[&big, "ok=1"]).await;
    assert_eq!(sent(&b, "/").await, "ok=1");
}
