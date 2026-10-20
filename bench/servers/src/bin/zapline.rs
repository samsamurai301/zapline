use serde::Serialize;
use zapline::metrics::Metrics;
use zapline::middleware::{SecurityHeaders, SetRequestId};
use zapline::prelude::*;
use zapline::telemetry::Trace;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Serialize)]
struct Message {
    message: &'static str,
}

#[derive(Serialize)]
struct User {
    id: u64,
    name: &'static str,
}

fn main() -> std::io::Result<()> {
    let workers: usize = std::env::var("WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let app = App::new()
        .get("/plaintext", || async { "Hello, World!" })
        .get("/json", || async {
            Json(Message {
                message: "Hello, World!",
            })
        })
        .get("/user/{id}", |Path(id): Path<u64>| async move {
            Json(User { id, name: "ada" })
        })
        .workers(workers);
    // MIDDLEWARE=1 adds commonly installed middleware (tracing stays disabled:
    // no subscriber), to measure what they cost per request.
    let app = if std::env::var("MIDDLEWARE").is_ok_and(|v| v == "1") {
        app.wrap(SetRequestId::new())
            .wrap(SecurityHeaders::default())
            .plugin(Metrics::new())
            .plugin(Trace::new())
    } else {
        app
    };
    app.run("127.0.0.1:3001")
}
