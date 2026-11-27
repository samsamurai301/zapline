//! Extractor rejections: status, stable `code`, a clean message and
//! structured `details` (JSON position and path, failing path parameter).
#![cfg(all(feature = "json", feature = "query", feature = "form"))]

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};
use zapline::prelude::*;
use zapline::test::{TestClient, TestResponse};

#[derive(Deserialize)]
#[allow(dead_code)]
struct Order {
    customer: Customer,
    items: Vec<Item>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Customer {
    email: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Item {
    name: String,
    qty: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Strict {
    a: u32,
}

#[derive(Deserialize)]
struct Page {
    page: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Cat,
    Dog,
}

#[derive(Deserialize)]
struct OrgUser {
    org: String,
    id: u32,
}

fn app() -> TestClient {
    App::new()
        .post("/orders", |Json(o): Json<Order>| async move {
            o.items.len().to_string()
        })
        .post(
            "/strict",
            |Json(s): Json<Strict>| async move { s.a.to_string() },
        )
        .post("/map", |Json(m): Json<HashMap<String, u32>>| async move {
            m.len().to_string()
        })
        .get("/page", |Query(p): Query<Page>| async move {
            p.page.to_string()
        })
        .post(
            "/page",
            |Form(p): Form<Page>| async move { p.page.to_string() },
        )
        .get("/users/{id}", |Path(id): Path<u64>| async move {
            id.to_string()
        })
        .get("/pets/{kind}", |Path(kind): Path<Kind>| async move {
            match kind {
                Kind::Cat => "meow",
                Kind::Dog => "woof",
            }
        })
        .get(
            "/orgs/{org}/users/{id}",
            |Path(p): Path<OrgUser>| async move { format!("{}/{}", p.org, p.id) },
        )
        .get(
            "/pairs/{a}/{b}",
            |Path((a, b)): Path<(String, u8)>| async move { format!("{a}{b}") },
        )
        .get("/one/{a}", |Path((a, b)): Path<(u32, u32)>| async move {
            (a + b).to_string()
        })
        .test()
}

async fn post_json(c: &TestClient, uri: &str, body: &str) -> TestResponse {
    c.post(uri)
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
}

/// `(message, code, details)` of an error response.
fn parts(r: &TestResponse) -> (String, String, Value) {
    let body: Value = r.json();
    (
        body["error"].as_str().unwrap_or_default().to_owned(),
        body["code"].as_str().unwrap_or_default().to_owned(),
        body["details"].clone(),
    )
}

#[tokio::test]
async fn valid_bodies_still_work() {
    let c = app();
    let r = post_json(
        &c,
        "/orders",
        r#"{"customer":{"email":"a@b.co"},"items":[{"name":"pen","qty":2}]}"#,
    )
    .await;
    r.assert_status(200);
    assert_eq!(r.text(), "1");
}

#[tokio::test]
async fn malformed_json_is_invalid_json() {
    let c = app();

    let r = post_json(&c, "/orders", r#"{"a" 1}"#).await;
    r.assert_status(422);
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_json");
    assert_eq!(msg, "invalid JSON body: expected `:`");
    assert_eq!(details, json!({ "line": 1, "column": 6 }));

    // Truncated input is a syntax (EOF) error too.
    let r = post_json(&c, "/orders", "{\"customer\":").await;
    r.assert_status(422);
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_json");
    assert_eq!(msg, "invalid JSON body: EOF while parsing a value");
    assert_eq!(details, json!({ "line": 1, "column": 12 }));

    let r = post_json(&c, "/orders", "").await;
    assert_eq!(parts(&r).1, "invalid_json");

    // Trailing garbage after a complete document.
    let r = post_json(&c, "/strict", r#"{"a":1} x"#).await;
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_json");
    assert_eq!(msg, "invalid JSON body: trailing characters");
    assert_eq!(details, json!({ "line": 1, "column": 9 }));
}

#[tokio::test]
async fn shape_errors_are_invalid_body_with_a_path() {
    let c = app();

    // Wrong type deep inside, on line 4.
    let body = r#"{
  "customer": {"email": "a@b.co"},
  "items": [
    {"name": "pen", "qty": "two"}
  ]
}"#;
    let r = post_json(&c, "/orders", body).await;
    r.assert_status(422);
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_body");
    assert_eq!(
        msg,
        r#"invalid request body: invalid type: string "two", expected u32"#
    );
    assert_eq!(
        details,
        json!({ "line": 4, "column": 32, "path": "items[0].qty" })
    );

    // Out-of-range value in the second array element.
    let r = post_json(
        &c,
        "/orders",
        r#"{"customer":{"email":"x"},"items":[{"name":"a","qty":1},{"name":"b","qty":-1}]}"#,
    )
    .await;
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_body");
    assert_eq!(
        msg,
        "invalid request body: invalid value: integer `-1`, expected u32"
    );
    assert_eq!(details["path"], "items[1].qty");

    // A missing field points at the object that lacks it.
    let r = post_json(&c, "/orders", r#"{"customer":{},"items":[]}"#).await;
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_body");
    assert_eq!(msg, "invalid request body: missing field `email`");
    assert_eq!(details["path"], "customer");

    // ...and at the top level there is no path.
    let r = post_json(&c, "/orders", r#"{"customer":{"email":"x"}}"#).await;
    let (msg, _, details) = parts(&r);
    assert_eq!(msg, "invalid request body: missing field `items`");
    assert_eq!(details, json!({ "line": 1, "column": 26 }));

    let r = post_json(&c, "/orders", r#""x""#).await;
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_body");
    assert_eq!(
        msg,
        r#"invalid request body: invalid type: string "x", expected struct Order"#
    );
    assert!(details.get("path").is_none(), "{details}");

    // Unknown fields point at the offending key.
    let r = post_json(&c, "/strict", r#"{"a":1,"b":2}"#).await;
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_body");
    assert_eq!(msg, "invalid request body: unknown field `b`, expected `a`");
    assert_eq!(details["path"], "b");

    // Keys are unescaped; structure inside strings is ignored.
    let r = post_json(&c, "/map", r#"{"a[0].b":1,"we\"ird":"x"}"#).await;
    let (_, code, details) = parts(&r);
    assert_eq!(code, "invalid_body");
    assert_eq!(details["path"], "we\"ird");
}

#[tokio::test]
async fn messages_never_repeat_the_position() {
    let c = app();
    for body in [
        "{",
        "{\"customer\":}",
        "[1,2",
        r#"{"customer":{"email":1},"items":[]}"#,
        "nul",
    ] {
        let r = post_json(&c, "/orders", body).await;
        r.assert_status(422);
        let (msg, _, details) = parts(&r);
        assert!(!msg.contains(" at line "), "{body}: {msg}");
        assert!(details["line"].as_u64().unwrap() >= 1, "{body}: {details}");
        assert!(details["column"].is_u64(), "{body}: {details}");
    }
}

#[tokio::test]
async fn wrong_content_type_is_415() {
    let c = app();
    let r = c.post("/orders").body("{}").send().await;
    r.assert_status(415);
    let (msg, code, _) = parts(&r);
    assert_eq!(code, "unsupported_media_type");
    assert_eq!(msg, "expected `Content-Type: application/json`");

    let r = c.post("/page").json(&json!({ "page": 1 })).send().await;
    r.assert_status(415);
    assert_eq!(parts(&r).1, "unsupported_media_type");
}

#[tokio::test]
async fn query_errors_are_400_invalid_query() {
    let c = app();
    c.get("/page?page=3").send().await.assert_status(200);

    let r = c.get("/page?page=three").send().await;
    r.assert_status(400);
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_query");
    assert_eq!(msg, "invalid query string: invalid digit found in string");
    assert!(details.is_null());

    let r = c.get("/page").send().await;
    r.assert_status(400);
    let (msg, code, _) = parts(&r);
    assert_eq!(code, "invalid_query");
    assert_eq!(msg, "invalid query string: missing field `page`");
}

#[tokio::test]
async fn form_errors_are_422_invalid_form() {
    let c = app();
    c.post("/page")
        .form(&[("page", "3")])
        .send()
        .await
        .assert_status(200);

    let r = c.post("/page").form(&[("page", "-1")]).send().await;
    r.assert_status(422);
    let (msg, code, _) = parts(&r);
    assert_eq!(code, "invalid_form");
    assert_eq!(msg, "invalid form body: invalid digit found in string");
}

#[tokio::test]
async fn path_errors_are_400_invalid_path_naming_the_param() {
    let c = app();
    c.get("/users/7").send().await.assert_status(200);

    let r = c.get("/users/abc").send().await;
    r.assert_status(400);
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_path");
    assert_eq!(
        msg,
        r#"invalid path parameter: `id` = "abc" is not a valid u64"#
    );
    assert_eq!(details, json!({ "param": "id" }));

    // Struct and tuple parameters blame the right one.
    let r = c.get("/orgs/acme/users/x").send().await;
    r.assert_status(400);
    assert_eq!(parts(&r).2, json!({ "param": "id" }));
    let r = c.get("/pairs/x/500").send().await;
    r.assert_status(400);
    assert_eq!(parts(&r).2, json!({ "param": "b" }));

    // Errors raised by the target type's own visitor are blamed too.
    let r = c.get("/pets/fish").send().await;
    r.assert_status(400);
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_path");
    assert!(
        msg.starts_with("invalid path parameter: unknown variant `fish`"),
        "{msg}"
    );
    assert_eq!(details, json!({ "param": "kind" }));
    c.get("/pets/dog").send().await.assert_status(200);

    // A shape mismatch with the route is nobody's parameter.
    let r = c.get("/one/1").send().await;
    r.assert_status(400);
    let (msg, code, details) = parts(&r);
    assert_eq!(code, "invalid_path");
    assert_eq!(
        msg,
        "invalid path parameter: expected 2 path parameters, route has 1"
    );
    assert!(details.is_null());
}

#[tokio::test]
async fn error_handlers_see_codes_and_details() {
    let c = App::new()
        .error_handler(|r: &zapline::ErrorReport| {
            let path = r
                .details
                .as_ref()
                .and_then(|d| d.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("-");
            format!(
                "{}|{}|{path}",
                r.status.as_u16(),
                r.code.as_deref().unwrap_or("-")
            )
            .into_response()
        })
        .post("/orders", |Json(o): Json<Order>| async move {
            o.items.len().to_string()
        })
        .test();
    let r = post_json(&c, "/orders", r#"{"customer":{"email":true},"items":[]}"#).await;
    assert_eq!(r.text(), "422|invalid_body|customer.email");
}

#[tokio::test]
async fn handlers_can_inspect_rejections() {
    let c = App::new()
        .get("/users/{id}", |id: Result<Path<u64>, Error>| async move {
            match id {
                Ok(Path(id)) => id.to_string(),
                Err(e) => format!("{}:{}", e.code().unwrap_or("-"), e.details().unwrap()),
            }
        })
        .test();
    assert_eq!(c.get("/users/1").send().await.text(), "1");
    assert_eq!(
        c.get("/users/x").send().await.text(),
        r#"invalid_path:{"param":"id"}"#
    );
}

#[test]
fn dev_mode_shows_the_real_500_message() {
    // ZAPLINE_DEV is read once per process, so check it in a child process.
    if std::env::var_os("ZAPLINE_DEV_CHILD").is_some() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let app = zapline::App::new().get("/", || async {
                Err::<(), _>(zapline::Error::internal("pool exhausted"))
            });
            let res = app.test().get("/").send().await;
            assert_eq!(res.status(), 500);
            assert!(res.text().contains("pool exhausted"), "{}", res.text());
        });
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "dev_mode_shows_the_real_500_message",
            "--exact",
            "--nocapture",
        ])
        .env("ZAPLINE_DEV_CHILD", "1")
        .env(zapline::ENV_DEV, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn form_helpers_handle_empty_inputs_and_checkboxes() {
    #[derive(Deserialize)]
    struct Profile {
        #[serde(default, deserialize_with = "zapline::extract::empty_as_none")]
        age: Option<u32>,
        #[serde(default, deserialize_with = "zapline::extract::checkbox")]
        public: bool,
    }
    let app = App::new().post("/", |Form(p): Form<Profile>| async move {
        format!("{:?} {}", p.age, p.public)
    });
    let client = app.test();
    let res = client.post("/").form(&[("age", "")]).send().await;
    assert_eq!(res.text(), "None false");
    let res = client
        .post("/")
        .form(&[("age", "42"), ("public", "on")])
        .send()
        .await;
    assert_eq!(res.text(), "Some(42) true");
    let res = client.post("/").form(&[("age", "x")]).send().await;
    assert_eq!(res.status(), 422);

    // The same struct works for JSON bodies.
    let app = App::new().post("/", |Json(p): Json<Profile>| async move {
        format!("{:?} {}", p.age, p.public)
    });
    let client = app.test();
    let res = client
        .post("/")
        .json(&json!({"age": 7, "public": true}))
        .send()
        .await;
    assert_eq!(res.text(), "Some(7) true");
    let res = client.post("/").json(&json!({"age": null})).send().await;
    assert_eq!(res.text(), "None false");
}
