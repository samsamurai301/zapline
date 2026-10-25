//! Arbitrary requests through a zapline app that uses most extractors and
//! middleware: path params, query, form, JSON, multipart, cookies (plain,
//! signed, private), sessions, CSRF, JWT, i18n, static files (ranges,
//! conditional requests), compression, CORS and path normalization.
//!
//! Fails on any panic and on any 5xx: every malformed input must be a
//! client error.
//!
//!     cargo +nightly fuzz run request -- -max_total_time=600
#![no_main]

use std::sync::OnceLock;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use serde::Deserialize;
use zapline::auth::{Authenticate, Claims, Csrf, CsrfToken, CurrentUser, JwtConfig};
use zapline::cookies::{CookieJar, Key, PrivateCookieJar, SignedCookieJar};
use zapline::extract::{checkbox, empty_as_none};
use zapline::fs::ServeDir;
use zapline::i18n::{I18n, Locale};
use zapline::middleware::{Compression, Cors, NormalizePath, SecurityHeaders};
use zapline::multipart::Multipart;
use zapline::prelude::*;
use zapline::session::{MemoryStore, Session, SessionLayer};
use zapline::test::TestClient;
use zapline::{Method, StatusCode};

const SECRET: &str = "fuzz-secret-that-is-at-least-32-bytes-long";

#[derive(Deserialize)]
#[allow(dead_code)]
struct Filter {
    page: Option<u32>,
    #[serde(default, deserialize_with = "empty_as_none")]
    q: Option<String>,
    #[serde(default, deserialize_with = "checkbox")]
    done: bool,
    sort: Option<Sort>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Sort {
    Asc,
    Desc,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Signup {
    email: String,
    age: Option<u8>,
    #[serde(default, deserialize_with = "checkbox")]
    terms: bool,
}

#[derive(Deserialize, serde::Serialize)]
struct Item {
    name: String,
    tags: Vec<String>,
    price: f64,
    meta: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    kind: Kind,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "type")]
enum Kind {
    Book { pages: u32 },
    Film { minutes: u16 },
}

fn static_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("zapline-fuzz-static");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("index.html"), "<h1>hi</h1>").unwrap();
    std::fs::write(dir.join("app.css"), "body{color:red}".repeat(200)).unwrap();
    std::fs::write(dir.join("sub/data.json"), "{\"a\":1}").unwrap();
    std::fs::write(dir.join(".env"), "SECRET=1").unwrap();
    dir
}

fn app() -> App {
    let jwt = JwtConfig::hs256(SECRET);
    let api = Routes::new()
        .get("/me", |u: CurrentUser<Claims>| async move { u.0.sub })
        .wrap(Authenticate::jwt_with(jwt));
    let i18n = I18n::default();
    App::new()
        .cookie_key(Key::from(&[7u8; 64]))
        .i18n(i18n)
        .get("/", || async { "ok" })
        .post("/", |body: String| async move { body })
        .get("/users/{id}", |Path(id): Path<u64>| async move {
            id.to_string()
        })
        .get(
            "/repos/{org}/{n}",
            |Path((org, n)): Path<(String, u32)>| async move { format!("{org}{n}") },
        )
        .get(
            "/files/{*rest}",
            |Path(rest): Path<String>| async move { rest },
        )
        .get("/query", |Query(f): Query<Filter>| async move {
            f.page.unwrap_or(0).to_string()
        })
        .post("/form", |Form(s): Form<Signup>| async move { s.email })
        .post("/json", |Json(item): Json<Item>| async move { Json(item) })
        .post("/upload", |mut mp: Multipart| async move {
            let mut n = 0usize;
            while let Some(f) = mp.next_field().await? {
                let _ = (
                    f.name().map(str::to_owned),
                    f.file_name().map(str::to_owned),
                );
                n += f.bytes().await?.len();
            }
            Ok::<_, Error>(n.to_string())
        })
        .get(
            "/cookies",
            |jar: CookieJar, signed: SignedCookieJar, private: PrivateCookieJar| async move {
                let a = jar.get("a").map(|c| c.value().len()).unwrap_or(0);
                let b = signed.get("b").map(|c| c.value().len()).unwrap_or(0);
                let c = private.get("c").map(|c| c.value().len()).unwrap_or(0);
                (jar.add(("a", "1")), format!("{a}{b}{c}"))
            },
        )
        .get("/session", |s: Session| async move {
            let n: u32 = s.get("n").unwrap_or(0);
            let _ = s.insert("n", n.wrapping_add(1));
            n.to_string()
        })
        .get("/csrf", |t: CsrfToken| async move { t.as_str().to_owned() })
        .post("/csrf", || async { "accepted" })
        .get("/t", |t: Locale| async move { t.t("hello") })
        .nest("/api", api)
        .static_files("/static", ServeDir::new(static_dir()))
        .wrap(
            Csrf::new()
                .exempt("/form")
                .exempt("/json")
                .exempt("/upload"),
        )
        .wrap(SessionLayer::new(MemoryStore::new()))
        .wrap(NormalizePath::trim_trailing_slash())
        .wrap(Compression::new())
        .wrap(Cors::permissive())
        .wrap(SecurityHeaders::default())
}

fn client() -> &'static (tokio::runtime::Runtime, TestClient) {
    static C: OnceLock<(tokio::runtime::Runtime, TestClient)> = OnceLock::new();
    C.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let c = rt.block_on(async { app().test() });
        (rt, c)
    })
}

const PATHS: &[&str] = &[
    "/",
    "/users/",
    "/repos/",
    "/files/",
    "/query",
    "/form",
    "/json",
    "/upload",
    "/cookies",
    "/session",
    "/csrf",
    "/t",
    "/api/me",
    "/static/",
    "/static/sub/",
    "/nope",
];
const HEADERS: &[&str] = &[
    "cookie",
    "authorization",
    "accept-language",
    "accept-encoding",
    "range",
    "if-range",
    "if-none-match",
    "if-modified-since",
    "if-match",
    "origin",
    "content-type",
    "x-csrf-token",
    "accept",
    "access-control-request-method",
    "access-control-request-headers",
    "x-forwarded-for",
    "forwarded",
    "x-request-id",
    "hx-request",
    "content-encoding",
    "sec-fetch-site",
    "referer",
    "host",
];
const CONTENT_TYPES: &[&str] = &[
    "application/json",
    "application/x-www-form-urlencoded",
    "multipart/form-data; boundary=X",
    "multipart/form-data; boundary=\"a b\"",
    "text/plain; charset=utf-8",
    "application/json; charset=latin1",
];
const METHODS: &[Method] = &[
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::DELETE,
    Method::HEAD,
    Method::OPTIONS,
    Method::PATCH,
];

#[derive(Arbitrary, Debug)]
struct Input {
    method: u8,
    path: u8,
    suffix: String,
    query: Option<String>,
    content_type: Option<u8>,
    headers: Vec<(u8, String)>,
    body: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let (rt, c) = client();
    let method = METHODS[input.method as usize % METHODS.len()].clone();
    let mut uri = format!(
        "{}{}",
        PATHS[input.path as usize % PATHS.len()],
        input.suffix
    );
    if let Some(q) = &input.query {
        uri.push('?');
        uri.push_str(q);
    }
    // Only URIs a server could have parsed reach the app.
    if uri.parse::<zapline::Uri>().is_err() {
        return;
    }
    let mut req = c.request(method, &uri);
    if let Some(ct) = input.content_type {
        req = req.header(
            "content-type",
            CONTENT_TYPES[ct as usize % CONTENT_TYPES.len()],
        );
    }
    for (name, value) in input.headers.iter().take(16) {
        let Ok(value) = zapline::HeaderValue::from_str(value) else {
            continue;
        };
        req = req.header(HEADERS[*name as usize % HEADERS.len()], value);
    }
    let r = rt.block_on(req.body(input.body).send());
    assert!(
        r.status() < StatusCode::INTERNAL_SERVER_ERROR,
        "{} for {uri}: {}",
        r.status(),
        r.text()
    );
});
