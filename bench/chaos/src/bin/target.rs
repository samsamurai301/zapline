//! The server under test: a zapline app with one route per kind of input
//! (path params, query, form, JSON, multipart, WebSocket, SSE, streaming,
//! panics), on default `ServerConfig` except for what the env sets.
//!
//!     ZL_HEADER_TIMEOUT_MS=2000 ZL_BODY_TIMEOUT_MS=3000 cargo run --release --bin target -- 127.0.0.1:8099

use std::collections::HashMap;
use std::time::Duration;

use zapline::multipart::Multipart;
use zapline::prelude::*;
use zapline::ws::{Message, WebSocketUpgrade};

async fn root() -> &'static str {
    "ok"
}

async fn user(Path(id): Path<u64>) -> String {
    format!("user {id}")
}

async fn files(Path(rest): Path<String>) -> String {
    format!("file {rest}")
}

async fn query(Query(q): Query<HashMap<String, String>>) -> String {
    format!("{}", q.len())
}

async fn form(Form(f): Form<HashMap<String, String>>) -> String {
    format!("{}", f.len())
}

async fn json(Json(v): Json<serde_json::Value>) -> Json<serde_json::Value> {
    Json(v)
}

async fn upload(mut mp: Multipart) -> Result<String, Error> {
    let mut total = 0usize;
    while let Some(field) = mp.next_field().await? {
        total += field.bytes().await?.len();
    }
    Ok(total.to_string())
}

async fn big() -> Vec<u8> {
    vec![b'x'; 4 * 1024 * 1024]
}

async fn boom() -> &'static str {
    panic!("handler panic on purpose")
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_millis(200)).await;
    "slow"
}

async fn echo_ws(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            if socket.send(msg).await.is_err() {
                break;
            }
        }
    })
}

fn env_ms(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()?
        .parse()
        .ok()
        .map(Duration::from_millis)
}

fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8099".into());
    let mut app = App::new()
        .get("/", root)
        .get("/users/{id}", user)
        .get("/files/{*rest}", files)
        .get("/query", query)
        .post("/form", form)
        .post("/json", json)
        .post("/upload", upload)
        .get("/big", big)
        .get("/panic", boom)
        .get("/slow", slow)
        .get("/ws", echo_ws)
        .http2(true);
    if std::env::var_os("ZL_CATCH_PANIC").is_some() {
        app = app.wrap(zapline::middleware::CatchPanic::new());
    }
    if let Ok(n) = std::env::var("ZL_MAX_CONNECTIONS") {
        app = app.max_connections(n.parse().expect("ZL_MAX_CONNECTIONS"));
    }
    let config = app.server_config_mut();
    config.banner = false;
    if let Some(t) = env_ms("ZL_HEADER_TIMEOUT_MS") {
        config.header_read_timeout = Some(t);
    }
    if let Some(t) = env_ms("ZL_BODY_TIMEOUT_MS") {
        config.body_read_timeout = Some(t);
    }
    if std::env::var_os("ZL_PIPELINE_FLUSH").is_some() {
        config.pipeline_flush = true;
    }
    if let Ok(w) = std::env::var("ZL_WORKERS") {
        config.workers = Some(w.parse().expect("ZL_WORKERS"));
    }
    app.run(&addr)
}
