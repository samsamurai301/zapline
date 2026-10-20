use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zapline::middleware::Cors;
use zapline::prelude::*;

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct User {
    id: u64,
    name: String,
}

#[get("/users/{id}")]
async fn get_user(Path(id): Path<u64>) -> Json<User> {
    Json(User {
        id,
        name: "ada".into(),
    })
}

#[post("/users")]
async fn create_user(Json(u): Json<User>) -> (StatusCode, Json<User>) {
    (StatusCode::CREATED, Json(u))
}

#[route("/multi", method = "GET", method = "PUT")]
async fn multi(m: Method) -> String {
    m.to_string()
}

#[tokio::test]
async fn macros_and_json() {
    let c = App::new().mount((get_user, create_user, multi)).test();

    let r = c.get("/users/42").send().await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(
        r.json::<User>(),
        User {
            id: 42,
            name: "ada".into()
        }
    );

    let u = User {
        id: 7,
        name: "bob".into(),
    };
    let r = c.post("/users").json(&u).send().await;
    assert_eq!(r.status(), 201);
    assert_eq!(r.json::<User>(), u);

    assert_eq!(c.put("/multi").send().await.text(), "PUT");
    assert_eq!(c.get("/multi").send().await.text(), "GET");
}

#[tokio::test]
async fn path_variants() {
    #[derive(Deserialize)]
    struct P {
        org: String,
        id: u32,
    }
    let c = App::new()
        .get(
            "/t/{a}/{b}",
            |Path((a, b)): Path<(String, u8)>| async move { format!("{a}-{b}") },
        )
        .get("/s/{org}/{id}", |Path(p): Path<P>| async move {
            format!("{}:{}", p.org, p.id)
        })
        .get(
            "/files/{*rest}",
            |Path(rest): Path<String>| async move { rest },
        )
        .get("/enc/{v}", |Path(v): Path<String>| async move { v })
        .test();

    assert_eq!(c.get("/t/x/5").send().await.text(), "x-5");
    assert_eq!(c.get("/t/x/500").send().await.status(), 400);
    assert_eq!(c.get("/s/acme/9").send().await.text(), "acme:9");
    assert_eq!(c.get("/files/a/b/c.txt").send().await.text(), "a/b/c.txt");
    assert_eq!(
        c.get("/enc/hello%20world").send().await.text(),
        "hello world"
    );
}

#[tokio::test]
async fn query_state_and_errors() {
    #[derive(Deserialize)]
    struct Q {
        n: Option<u32>,
    }
    #[derive(Clone)]
    struct Counter(Arc<AtomicUsize>);

    async fn hit(State(c): State<Counter>, Query(q): Query<Q>) -> Result<String> {
        let n = q.n.ok_or_else(|| Error::bad_request("missing n"))?;
        let total = c.0.fetch_add(n as usize, Ordering::Relaxed) + n as usize;
        Ok(total.to_string())
    }
    async fn boom() -> Result<&'static str> {
        let _: u32 = "nope".parse()?; // any std error -> 500
        Ok("unreachable")
    }

    let c = App::new()
        .state(Counter(Arc::new(AtomicUsize::new(0))))
        .get("/hit", hit)
        .get("/boom", boom)
        .test();

    assert_eq!(c.get("/hit?n=3").send().await.text(), "3");
    assert_eq!(c.get("/hit?n=4").send().await.text(), "7");
    let r = c.get("/hit").send().await;
    assert_eq!(r.status(), 400);
    assert_eq!(r.text(), r#"{"error":"missing n"}"#);
    let r = c.get("/boom").send().await;
    assert_eq!(r.status(), 500);
    assert_eq!(r.text(), r#"{"error":"internal server error"}"#);
}

#[tokio::test]
async fn not_found_and_method_not_allowed() {
    let c = App::new()
        .get("/a", || async { "a" })
        .post("/a", || async { "a" })
        .test();
    assert_eq!(c.get("/nope").send().await.status(), 404);
    let r = c.delete("/a").send().await;
    assert_eq!(r.status(), 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD, POST"));
}

#[tokio::test]
async fn fallback_handler() {
    let c = App::new()
        .fallback(|uri: Uri| async move { (StatusCode::NOT_FOUND, format!("no {}", uri.path())) })
        .test();
    let r = c.get("/x").send().await;
    assert_eq!(r.status(), 404);
    assert_eq!(r.text(), "no /x");
}

#[tokio::test]
async fn middleware_order_and_groups() {
    async fn tag(mut req: Request, next: Next) -> Response {
        req.extensions_mut().insert("from-mw");
        let mut res = next.run(req).await;
        res.headers_mut()
            .insert("x-global", HeaderValue::from_static("1"));
        res
    }
    async fn admin_only(req: Request, next: Next) -> Response {
        if req.headers().get("x-admin").is_none() {
            return Error::unauthorized().into_response();
        }
        next.run(req).await
    }

    let admin = Routes::new()
        .get(
            "/stats",
            |Extension(v): Extension<&'static str>| async move { v },
        )
        .wrap(admin_only);

    let c = App::new()
        .wrap(tag)
        .get("/public", || async { "pub" })
        .nest("/admin", admin)
        .test();

    let r = c.get("/public").send().await;
    assert_eq!(r.text(), "pub");
    assert_eq!(r.header("x-global"), Some("1"));
    assert_eq!(c.get("/admin/stats").send().await.status(), 401);
    let r = c.get("/admin/stats").header("x-admin", "y").send().await;
    assert_eq!(r.text(), "from-mw");
    // Global middleware still runs on 404s.
    assert_eq!(c.get("/missing").send().await.header("x-global"), Some("1"));
}

#[tokio::test]
async fn body_limit_and_content_type() {
    let c = App::new()
        .body_limit(8)
        .post("/echo", |b: Bytes| async move { b })
        .post(
            "/json",
            |Json(v): Json<serde_json_value::Value>| async move { Json(v) },
        )
        .test();
    assert_eq!(
        c.post("/echo").body("12345678").send().await.text(),
        "12345678"
    );
    assert_eq!(c.post("/echo").body("123456789").send().await.status(), 413);
    assert_eq!(c.post("/json").body("{}").send().await.status(), 415);
}

// Tiny stand-in so the test crate does not need serde_json directly.
mod serde_json_value {
    #[derive(serde::Serialize, serde::Deserialize)]
    pub struct Value {}
}

#[tokio::test]
async fn cors_preflight() {
    let c = App::new()
        .wrap(Cors::permissive())
        .get("/x", || async { "x" })
        .test();
    let r = c
        .request(Method::OPTIONS, "/x")
        .header("origin", "https://a.com")
        .header("access-control-request-method", "GET")
        .send()
        .await;
    assert_eq!(r.status(), 204);
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    let r = c.get("/x").header("origin", "https://a.com").send().await;
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
}

#[tokio::test]
async fn real_socket_roundtrip() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(App::new().get("/", || async { "hi" }).serve(listener));

    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    // Two pipelined requests on one connection.
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
    assert!(out.contains("content-length: 2"), "{out}");
    assert!(out.contains("HTTP/1.1 404 Not Found"), "{out}");
}

// Mirrors the custom-extractor example in the README.
struct ApiKey(String);

impl zapline::extract::FromRequestParts for ApiKey {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|v| ApiKey(v.to_owned()))
            .ok_or_else(Error::unauthorized)
    }
}

#[tokio::test]
async fn custom_extractor() {
    let c = App::new()
        .get("/k", |ApiKey(k): ApiKey| async move { k })
        .test();
    assert_eq!(c.get("/k").send().await.status(), 401);
    assert_eq!(
        c.get("/k")
            .header("x-api-key", "s3cret")
            .send()
            .await
            .text(),
        "s3cret"
    );
}
