//! The in-process `TestClient`: cookie persistence, multipart bodies, query
//! strings, response helpers and assertions.

use std::panic::{catch_unwind, AssertUnwindSafe};

use serde::{Deserialize, Serialize};
use zapline::prelude::*;
use zapline::test::{MultipartForm, SetCookie, TestClient, TestResponse};

/// A response that sets each of `cookies`.
fn set_cookies(cookies: &[&'static str]) -> Response {
    let mut res = "ok".into_response();
    for c in cookies {
        res.headers_mut()
            .append(header::SET_COOKIE, HeaderValue::from_static(c));
    }
    res
}

/// Echoes every `Cookie` header, separated by ` | `.
async fn echo_cookies(headers: HeaderMap) -> String {
    headers
        .get_all(header::COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn cookie_app() -> App {
    App::new()
        .get("/login", || async {
            set_cookies(&["sid=abc; Path=/; HttpOnly; SameSite=Lax"])
        })
        .get("/rotate", || async { set_cookies(&["sid=def; Path=/"]) })
        .get("/logout", || async {
            set_cookies(&["sid=; Path=/; Max-Age=0"])
        })
        // No `Path`: defaults to the request's directory, `/`.
        .get("/prefs", || async {
            set_cookies(&["theme=dark", "lang=en; Max-Age=3600"])
        })
        .get("/forget", || async {
            set_cookies(&["theme=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT"])
        })
        .get("/admin/login", || async {
            set_cookies(&["admin=1; Path=/admin"])
        })
        // No `Path`: defaults to `/account/settings`.
        .get("/account/settings/save", || async {
            set_cookies(&["acct=1"])
        })
        .fallback(echo_cookies)
}

async fn echo(c: &TestClient, path: &str) -> String {
    c.get(path).send().await.text()
}

#[tokio::test]
async fn app_test_is_stateless() {
    let c = cookie_app().test();
    c.get("/login").send().await.assert_status(200);
    assert_eq!(echo(&c, "/echo").await, "");
    assert_eq!(c.cookie("sid"), None);
}

#[tokio::test]
async fn with_cookies_sends_stored_cookies_in_one_header() {
    let c = cookie_app().test().with_cookies();
    c.get("/login").send().await;
    c.get("/prefs").send().await;
    assert_eq!(echo(&c, "/echo").await, "sid=abc; theme=dark; lang=en");
    assert_eq!(c.cookie("sid").as_deref(), Some("abc"));
    assert_eq!(c.cookie("lang").as_deref(), Some("en"));
    assert_eq!(c.cookie("nope"), None);

    // Same name and path: replaced in place.
    c.get("/rotate").send().await;
    assert_eq!(echo(&c, "/echo").await, "sid=def; theme=dark; lang=en");
}

#[tokio::test]
async fn removals_delete_cookies() {
    let c = cookie_app().test().with_cookies();
    c.get("/login").send().await;
    c.get("/prefs").send().await;

    // `Max-Age=0`.
    c.get("/logout").send().await;
    assert_eq!(c.cookie("sid"), None);
    assert_eq!(echo(&c, "/echo").await, "theme=dark; lang=en");

    // `Expires` in the past.
    c.get("/forget").send().await;
    assert_eq!(echo(&c, "/echo").await, "lang=en");

    // Deleting a cookie the client never had is harmless.
    c.get("/logout").send().await;
    assert_eq!(echo(&c, "/echo").await, "lang=en");
}

#[tokio::test]
async fn cookies_are_scoped_by_path() {
    let c = cookie_app().test().with_cookies();
    c.get("/login").send().await;
    c.get("/admin/login").send().await;
    c.get("/account/settings/save").send().await;

    // Longest path first.
    assert_eq!(echo(&c, "/admin").await, "admin=1; sid=abc");
    assert_eq!(echo(&c, "/admin/users").await, "admin=1; sid=abc");
    assert_eq!(echo(&c, "/administrator").await, "sid=abc");
    assert_eq!(echo(&c, "/").await, "sid=abc");

    // Default path from the request that set it.
    assert_eq!(echo(&c, "/account/settings").await, "acct=1; sid=abc");
    assert_eq!(
        echo(&c, "/account/settings/emails").await,
        "acct=1; sid=abc"
    );
    assert_eq!(echo(&c, "/account/other").await, "sid=abc");

    // The query string is not part of the path.
    assert_eq!(echo(&c, "/admin?x=/").await, "admin=1; sid=abc");
}

#[tokio::test]
async fn explicit_cookie_headers_take_precedence() {
    let c = cookie_app().test().with_cookies();
    c.get("/login").send().await;

    let r = c.get("/echo").header("cookie", "extra=1").send().await;
    assert_eq!(r.text(), "extra=1; sid=abc");

    let r = c
        .get("/echo")
        .header("cookie", "sid=manual; extra=1")
        .send()
        .await;
    assert_eq!(r.text(), "sid=manual; extra=1");

    // The store itself is unchanged.
    assert_eq!(echo(&c, "/echo").await, "sid=abc");
}

#[tokio::test]
async fn clones_share_the_store_and_each_call_starts_a_new_one() {
    let alice = cookie_app().test().with_cookies();
    let alice2 = alice.clone();
    alice.get("/login").send().await;
    assert_eq!(echo(&alice2, "/echo").await, "sid=abc");

    // Same app, a separate browser.
    let bob = alice.clone().with_cookies();
    assert_eq!(echo(&bob, "/echo").await, "");
    bob.get("/admin/login").send().await;
    assert_eq!(echo(&alice, "/admin").await, "sid=abc");
}

#[tokio::test]
async fn response_cookie_and_header_helpers() {
    let c = cookie_app().test();
    let r = c.get("/prefs").send().await;
    assert_eq!(
        r.header_all("set-cookie"),
        ["theme=dark", "lang=en; Max-Age=3600"]
    );
    assert_eq!(r.header_all("x-missing"), Vec::<&str>::new());

    let lang = r.cookie("lang").unwrap();
    assert_eq!((lang.name(), lang.value()), ("lang", "en"));
    assert_eq!(lang.max_age(), Some(3600));
    assert_eq!(lang.path(), None);
    assert!(!lang.is_removal());
    assert!(r.cookie("sid").is_none());

    let r = c.get("/login").send().await;
    let sid = r.cookie("sid").unwrap();
    assert_eq!(sid.path(), Some("/"));
    assert_eq!(sid.same_site(), Some("Lax"));
    assert!(sid.http_only());
    assert!(!sid.secure());
    assert_eq!(sid.attr("httponly"), Some(""));
    assert_eq!(
        format!("{sid:?}"),
        "SetCookie(sid=abc; Path=/; HttpOnly; SameSite=Lax)"
    );

    let r = c.get("/logout").send().await;
    assert!(r.cookie("sid").unwrap().is_removal());
    let r = c.get("/forget").send().await;
    let theme = r.cookie("theme").unwrap();
    assert!(theme.is_removal());
    assert!(theme.expires().is_some());

    // The last header for a name wins.
    let c = App::new()
        .get("/", || async { set_cookies(&["a=1", "b=2", "a=3"]) })
        .test();
    assert_eq!(c.get("/").send().await.cookie("a").unwrap().value(), "3");

    assert!(SetCookie::parse("garbage").is_none());
}

#[cfg(feature = "cookies")]
#[tokio::test]
async fn cookie_jar_responses_round_trip() {
    use zapline::cookies::Cookie;

    let c = App::new()
        .get("/set", |jar: CookieJar| async move {
            jar.add(Cookie::new("flash", "saved"))
        })
        .get(
            "/clear",
            |jar: CookieJar| async move { jar.remove("flash") },
        )
        .get("/read", |jar: CookieJar| async move {
            jar.get("flash")
                .map(|c| c.value().to_owned())
                .unwrap_or_default()
        })
        .test()
        .with_cookies();

    c.get("/set").send().await.assert_status(200);
    assert_eq!(c.get("/read").send().await.text(), "saved");
    let r = c.get("/clear").send().await;
    assert!(r.cookie("flash").unwrap().is_removal());
    assert_eq!(c.get("/read").send().await.text(), "");
}

#[cfg(feature = "cookies")]
#[tokio::test]
async fn session_login_flow() {
    use serde_json::json;
    use zapline::cookies::Key;
    use zapline::session::{MemoryStore, Session, SessionLayer};

    #[derive(Deserialize)]
    struct Login {
        username: String,
        password: String,
    }

    async fn login(session: Session, Json(form): Json<Login>) -> Result<&'static str> {
        if form.password != "hunter2" {
            return Err(Error::unauthorized());
        }
        session.regenerate();
        session.insert("user", &form.username)?;
        Ok("welcome")
    }

    async fn me(session: Session) -> Result<String> {
        session
            .get::<String>("user")
            .ok_or_else(Error::unauthorized)
    }

    async fn logout(session: Session) -> &'static str {
        session.destroy();
        "bye"
    }

    let app = App::new()
        .cookie_key(Key::from(&[7u8; 64]))
        .wrap(SessionLayer::new(MemoryStore::new()))
        .post("/login", login)
        .get("/me", me)
        .post("/logout", logout);
    let browser = app.test().with_cookies();
    let stranger = browser.clone().with_cookies();

    browser.get("/me").send().await.assert_status(401);
    browser
        .post("/login")
        .json(&json!({ "username": "ada", "password": "wrong" }))
        .send()
        .await
        .assert_status(401);
    let r = browser
        .post("/login")
        .json(&json!({ "username": "ada", "password": "hunter2" }))
        .send()
        .await;
    r.assert_status(200);
    let set = r.cookie("zapline.sid").expect("session cookie");
    assert!(set.http_only() && set.secure());

    let sid = browser
        .cookie("zapline.sid")
        .expect("stored session cookie");
    assert_eq!(sid, set.value());
    let r = browser.get("/me").send().await;
    r.assert_status(200);
    assert_eq!(r.text(), "ada");
    assert_eq!(browser.get("/me").send().await.text(), "ada");
    stranger.get("/me").send().await.assert_status(401);

    browser.post("/logout").send().await.assert_status(200);
    assert_eq!(browser.cookie("zapline.sid"), None);
    browser.get("/me").send().await.assert_status(401);

    // The old cookie no longer opens the session.
    let r = stranger
        .get("/me")
        .header("cookie", format!("zapline.sid={sid}"))
        .send()
        .await;
    r.assert_status(401);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_upload() {
    use zapline::multipart::Multipart;

    /// `name|file|type|size|text` per field.
    async fn describe(mut form: Multipart) -> Result<String> {
        let mut lines = Vec::new();
        while let Some(field) = form.next_field().await? {
            let name = field.name().unwrap_or("?").to_owned();
            let file = field.file_name().unwrap_or("-").to_owned();
            let ct = field.content_type().unwrap_or("-").to_owned();
            let data = field.bytes().await?;
            let text = std::str::from_utf8(&data).unwrap_or("<binary>");
            lines.push(format!("{name}|{file}|{ct}|{}|{text}", data.len()));
        }
        Ok(lines.join("\n"))
    }

    let c = App::new().post("/upload", describe).test();
    let form = MultipartForm::new()
        .text("title", "Holiday")
        .text("note", "caf\u{e9}\r\n--second line")
        .file(
            "photo",
            "beach photo.png",
            "image/png",
            vec![0x89, b'P', b'N', b'G', 0, 255],
        )
        .file("notes", "notes.txt", "text/plain", "--zapline-x\r\n")
        .text("empty", "");
    let r = c.post("/upload").multipart(form).send().await;
    r.assert_status(200);
    assert_eq!(
        r.text(),
        "title|-|-|7|Holiday\n\
         note|-|-|20|caf\u{e9}\r\n--second line\n\
         photo|beach photo.png|image/png|6|<binary>\n\
         notes|notes.txt|text/plain|13|--zapline-x\r\n\n\
         empty|-|-|0|"
    );

    // An empty form is valid.
    let r = c
        .post("/upload")
        .multipart(MultipartForm::new())
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.text(), "");
}

#[tokio::test]
async fn multipart_content_type_and_body() {
    let c = App::new()
        .post("/raw", |headers: HeaderMap, body: Bytes| async move {
            let ct = headers[header::CONTENT_TYPE].to_str().unwrap().to_owned();
            format!("{ct}\n{}", String::from_utf8_lossy(&body))
        })
        .test();
    let form = MultipartForm::new().text("a", "1").file(
        "f",
        "x.bin",
        "application/octet-stream",
        vec![1, 2],
    );
    let text = c.post("/raw").multipart(form).send().await.text();
    let (ct, body) = text.split_once('\n').unwrap();
    let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
    assert_eq!(
        body,
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"f\"; filename=\"x.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n\u{1}\u{2}\r\n\
             --{boundary}--\r\n"
        )
    );
}

#[cfg(feature = "query")]
#[tokio::test]
async fn query_strings() {
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Search {
        q: String,
        page: u32,
    }

    let c = App::new()
        .get("/uri", |uri: Uri| async move { uri.to_string() })
        .get("/search", |Query(s): Query<Search>| async move {
            format!("{}:{}", s.q, s.page)
        })
        .test();

    let search = Search {
        q: "rust & web".into(),
        page: 2,
    };
    let r = c.get("/search").query(&search).send().await;
    assert_eq!(r.text(), "rust & web:2");

    let uri = |r: TestResponse| r.text();
    assert_eq!(
        uri(c.get("/uri").query(&search).send().await),
        "/uri?q=rust+%26+web&page=2"
    );
    // Appends to an existing query, however it ends.
    assert_eq!(
        uri(c.get("/uri?a=1").query(&[("b", "2")]).send().await),
        "/uri?a=1&b=2"
    );
    assert_eq!(
        uri(c.get("/uri?").query(&[("b", "2")]).send().await),
        "/uri?b=2"
    );
    assert_eq!(
        uri(c.get("/uri?a=1&").query(&[("b", "2")]).send().await),
        "/uri?a=1&b=2"
    );
    // Calls accumulate; an empty query changes nothing.
    assert_eq!(
        uri(c
            .get("/uri")
            .query(&[("a", "1")])
            .query(&[("b", "2")])
            .query(&Vec::<(String, String)>::new())
            .send()
            .await),
        "/uri?a=1&b=2"
    );
}

/// The message of the panic `f` raises.
fn panic_message(f: impl FnOnce()) -> String {
    let err = catch_unwind(AssertUnwindSafe(f)).expect_err("expected a panic");
    err.downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

#[tokio::test]
async fn assertions_chain_and_explain_failures() {
    let c = App::new()
        .post("/items", || async { (StatusCode::CREATED, "made") })
        .get("/big", || async { "x".repeat(5000) })
        .get("/empty", || async { StatusCode::NO_CONTENT })
        .test();

    let r = c.post("/items").send().await;
    r.assert_status(201)
        .assert_status(StatusCode::CREATED)
        .assert_header("content-type", "text/plain; charset=utf-8");

    let msg = panic_message(|| {
        r.assert_status(StatusCode::OK);
    });
    assert_eq!(
        msg,
        "expected status 200, got 201 Created\nresponse body: made"
    );

    let msg = panic_message(|| {
        r.assert_header("content-type", "application/json");
    });
    assert_eq!(
        msg,
        "expected header `content-type: application/json`, got [\"text/plain; charset=utf-8\"] \
         (status 201 Created)\nresponse body: made"
    );

    // Error bodies explain themselves.
    let r = c.post("/missing").send().await;
    let msg = panic_message(|| {
        r.assert_status(200);
    });
    assert!(
        msg.starts_with("expected status 200, got 404 Not Found\n"),
        "{msg}"
    );
    assert!(msg.contains(r#"{"error":"not found"}"#), "{msg}");

    // Long bodies are cut; empty ones say so.
    let r = c.get("/big").send().await;
    let msg = panic_message(|| {
        r.assert_status(500);
    });
    assert!(msg.ends_with("... (5000 bytes total)"), "{msg}");
    assert!(msg.len() < 2200, "{}", msg.len());
    let r = c.get("/empty").send().await;
    let msg = panic_message(|| {
        r.assert_status(200);
    });
    assert!(msg.ends_with("response body: <empty>"), "{msg}");
}
