//! Cookie jars: parsing, delta semantics, signed/private jars.
#![cfg(feature = "cookies")]

use serde_json::Value;
use zapline::cookies::{
    secure_cookie, time::Duration, Cookie, CookieJar, Key, PrivateCookieJar, SameSite,
    SignedCookieJar,
};
use zapline::prelude::*;
use zapline::test::TestResponse;

fn set_cookies(r: &TestResponse) -> Vec<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

/// `name=value` of a `Set-Cookie` header (what a browser sends back).
fn pair(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

#[tokio::test]
async fn parses_all_cookie_headers() {
    let c = App::new()
        .get("/", |jar: CookieJar| async move {
            let mut v: Vec<String> = jar
                .iter()
                .map(|c| format!("{}={}", c.name(), c.value()))
                .collect();
            v.sort();
            v.join("&")
        })
        .test();
    let r = c
        .get("/")
        .header("cookie", "a=1; b=hello%20world")
        .header("cookie", "c=3; a=dup; ;bad; =x")
        .send()
        .await;
    // Percent-decoded, first duplicate wins, malformed pairs skipped.
    assert_eq!(r.text(), "a=1&b=hello world&c=3");
    assert_eq!(c.get("/").send().await.text(), "");
}

#[tokio::test]
async fn caps_the_number_of_cookies() {
    let c = App::new()
        .get("/", |jar: CookieJar| async move {
            jar.iter().count().to_string()
        })
        .test();
    let header: Vec<String> = (0..400).map(|i| format!("c{i}=v")).collect();
    let r = c.get("/").header("cookie", header.join("; ")).send().await;
    assert_eq!(r.text(), "256");
}

#[tokio::test]
async fn only_changes_are_sent() {
    let c = App::new()
        .get("/noop", |jar: CookieJar| async move { (jar, "same") })
        .get("/add", |jar: CookieJar| async move {
            (jar.add(Cookie::new("new", "v; 1")), "added")
        })
        .get("/replace", |jar: CookieJar| async move {
            (jar.add(("a", "2")), "replaced")
        })
        .get("/remove", |jar: CookieJar| async move {
            (jar.remove("a").remove("ghost"), "removed")
        })
        .get("/add-remove", |jar: CookieJar| async move {
            (jar.add(("tmp", "x")).remove("tmp"), "nothing")
        })
        .get("/status", |jar: CookieJar| async move {
            (StatusCode::CREATED, jar.add(("s", "1")), "created")
        })
        .get(
            "/bare",
            |jar: CookieJar| async move { jar.add(("only", "1")) },
        )
        .test();
    let send = |path: &'static str| {
        let c = c.clone();
        async move { c.get(path).header("cookie", "a=1; b=2").send().await }
    };

    assert!(set_cookies(&send("/noop").await).is_empty());

    let r = send("/add").await;
    assert_eq!(r.text(), "added");
    assert_eq!(set_cookies(&r), ["new=v%3B%201"]);

    assert_eq!(set_cookies(&send("/replace").await), ["a=2"]);

    let removed = set_cookies(&send("/remove").await);
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(removed[0].starts_with("a=;"), "{removed:?}");
    assert!(removed[0].contains("Max-Age=0"), "{removed:?}");
    assert!(removed[0].contains("Path=/"), "{removed:?}");

    assert!(set_cookies(&send("/add-remove").await).is_empty());

    let r = send("/status").await;
    assert_eq!(r.status(), 201);
    assert_eq!(set_cookies(&r), ["s=1"]);

    let r = send("/bare").await;
    assert_eq!(r.status(), 200);
    assert_eq!(set_cookies(&r), ["only=1"]);
}

#[test]
fn secure_cookie_defaults() {
    let c = secure_cookie("sid", "abc").build();
    assert_eq!(c.http_only(), Some(true));
    assert_eq!(c.secure(), Some(true));
    assert_eq!(c.same_site(), Some(SameSite::Lax));
    assert_eq!(c.path(), Some("/"));
    let c = secure_cookie("sid", "abc")
        .same_site(SameSite::Strict)
        .max_age(Duration::hours(1))
        .build();
    assert_eq!(c.same_site(), Some(SameSite::Strict));
    assert_eq!(c.max_age(), Some(Duration::hours(1)));
}

#[test]
fn jar_api_without_requests() {
    let jar = CookieJar::new()
        .add(("a", "1"))
        .add(secure_cookie("b", "2"));
    assert_eq!(jar.get("a").unwrap().value(), "1");
    assert_eq!(jar.delta().count(), 2);
    let jar = jar.remove("a");
    assert!(jar.get("a").is_none());
    assert_eq!(format!("{jar:?}"), r#"["b"]"#);
}

fn key() -> Key {
    Key::from(&[7u8; 64])
}

#[tokio::test]
async fn signed_jar_detects_tampering() {
    let c = App::new()
        .cookie_key(key())
        .get("/set", |jar: SignedCookieJar| async move {
            (jar.add(secure_cookie("user", "ada")), "set")
        })
        .get("/get", |jar: SignedCookieJar| async move {
            jar.get("user")
                .map_or("none".to_owned(), |c| c.value().to_owned())
        })
        .get("/clear", |jar: SignedCookieJar| async move {
            jar.remove("user")
        })
        .test();

    let set = set_cookies(&c.get("/set").send().await);
    assert_eq!(set.len(), 1);
    assert!(set[0].contains("HttpOnly") && set[0].contains("Secure"));
    let cookie = pair(&set[0]).to_owned();
    // Signed, not encrypted: the value is visible but carries a MAC.
    assert!(
        cookie.starts_with("user=") && cookie.ends_with("ada"),
        "{cookie}"
    );
    assert_ne!(cookie, "user=ada");

    let get = |cookie: String| {
        let c = c.clone();
        async move { c.get("/get").header("cookie", cookie).send().await.text() }
    };
    assert_eq!(get(cookie.clone()).await, "ada");
    assert_eq!(get(cookie.replace("ada", "bob")).await, "none");
    assert_eq!(get("user=ada".into()).await, "none");
    assert_eq!(get(String::new()).await, "none");

    // Another key cannot verify it.
    let other = App::new()
        .cookie_key(Key::generate())
        .get("/get", |jar: SignedCookieJar| async move {
            jar.get("user")
                .map_or("none".to_owned(), |c| c.value().to_owned())
        })
        .test();
    let r = other
        .get("/get")
        .header("cookie", cookie.clone())
        .send()
        .await;
    assert_eq!(r.text(), "none");

    let r = c.get("/clear").header("cookie", cookie).send().await;
    let removal = set_cookies(&r);
    assert!(removal[0].starts_with("user=;") && removal[0].contains("Max-Age=0"));
}

#[tokio::test]
async fn private_jar_hides_and_protects_values() {
    let c = App::new()
        .cookie_key(key())
        .get("/set", |jar: PrivateCookieJar| async move {
            (jar.add(("card", "4111 1111; secret")), "set")
        })
        .get("/get", |jar: PrivateCookieJar| async move {
            jar.get("card")
                .map_or("none".to_owned(), |c| c.value().to_owned())
        })
        .test();
    let set = set_cookies(&c.get("/set").send().await);
    let cookie = pair(&set[0]).to_owned();
    assert!(
        !cookie.contains("4111") && !cookie.contains("secret"),
        "{cookie}"
    );

    let r = c.get("/get").header("cookie", cookie.clone()).send().await;
    assert_eq!(r.text(), "4111 1111; secret");

    // Flip one character of the ciphertext.
    let (name, value) = cookie.split_once('=').unwrap();
    let mut bytes = value.as_bytes().to_vec();
    let i = bytes.len() / 2;
    bytes[i] = if bytes[i] == b'A' { b'B' } else { b'A' };
    let tampered = format!("{name}={}", String::from_utf8(bytes).unwrap());
    let r = c.get("/get").header("cookie", tampered).send().await;
    assert_eq!(r.text(), "none");
    let r = c
        .get("/get")
        .header("cookie", "card=plaintext")
        .send()
        .await;
    assert_eq!(r.text(), "none");
}

#[tokio::test]
async fn secure_jars_need_a_key() {
    let c = App::new()
        .get("/s", |_: SignedCookieJar| async { "x" })
        .get("/p", |_: PrivateCookieJar| async { "x" })
        .test();
    for path in ["/s", "/p"] {
        let r = c.get(path).send().await;
        assert_eq!(r.status(), 500);
        assert_eq!(r.json::<Value>()["code"], "cookie_key_missing");
    }
}

#[test]
fn jar_conversions() {
    let jar = CookieJar::new().into_signed(key()).add(("a", "1"));
    let raw = jar.clone().into_inner();
    assert_ne!(raw.get("a").unwrap().value(), "1");
    assert_eq!(jar.get("a").unwrap().value(), "1");
    let private = CookieJar::new().into_private(key()).add(("b", "2"));
    assert_eq!(private.get("b").unwrap().value(), "2");
    assert_eq!(private.delta().count(), 1);
}
