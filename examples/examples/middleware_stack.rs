//! A production-style middleware stack.
//!
//!     cargo run -p zapline-examples --example middleware_stack
//!     curl -i localhost:8080/api/items/
//!     curl -i --compressed localhost:8080/api/report
//!     for i in $(seq 7); do curl -s -o /dev/null -w "%{http_code}\n" -X POST localhost:8080/api/login; done
//!
//! Global middleware runs before routing, outermost first; route middleware
//! (`wrap_route`) runs after routing and sees per-route metadata such as
//! rate limit policies.

use std::time::Duration;

use zapline::middleware::{
    CatchPanic, Compression, ConcurrencyLimit, Cors, Logger, NormalizePath, RateLimit, RequestId,
    SecurityHeaders, SetRequestId, Timeout,
};
use zapline::prelude::*;

const MINUTE: Duration = Duration::from_secs(60);

async fn items(id: RequestId) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "items": ["a", "b", "c"], "request_id": id.as_str() }))
}

async fn report() -> String {
    // Large and repetitive: a good compression candidate.
    (1..=500).map(|i| format!("row {i}: ok\n")).collect()
}

async fn login() -> &'static str {
    "welcome"
}

async fn boom() -> &'static str {
    panic!("this handler is broken")
}

fn app() -> App {
    let api = Routes::new()
        .get("/items", items)
        .get("/report", report)
        .post("/login", login)
        .rate_limit(5, MINUTE) // per client, separate bucket
        .get("/boom", boom);

    App::new()
        .wrap(SetRequestId::new())
        .wrap(Logger)
        .wrap(SecurityHeaders::default())
        .wrap(CatchPanic::new())
        .wrap(Cors::origins(["http://localhost:3000"]).expose_headers("x-request-id"))
        .wrap(NormalizePath::trim_trailing_slash())
        .wrap(ConcurrencyLimit::new(1024).queue_timeout(Duration::from_millis(100)))
        .wrap(Timeout::new(Duration::from_secs(30)))
        .wrap(Compression::new())
        .nest("/api", api)
        .get("/healthz", || async { "ok" })
        // Default limit for every route; `/api/login` overrides it.
        .wrap_route(
            RateLimit::per_ip(120, MINUTE)
                .trust_forwarded_for(["127.0.0.1", "10.0.0.0/8"])
                .exempt(|req: &Request| req.path() == "/healthz"),
        )
}

fn main() -> std::io::Result<()> {
    println!("listening on http://localhost:8080");
    app().run("0.0.0.0:8080")
}
