use bench_servers as tfb;
use serde::Serialize;
use zapline::db::{Database, Db};
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
        .workers(tfb::workers());
    // NO_DB=1 serves only the endpoints above (no Postgres needed).
    let app = if std::env::var("NO_DB").is_ok_and(|v| v == "1") {
        app
    } else {
        app.database(
            Database::postgres(tfb::database_url())
                .max_connections(tfb::pool_size())
                .migrate(false),
        )
        .get("/db", |db: Db| async move {
            tfb::single_query(&db).await.map(Json).map_err(Error::from)
        })
        .get("/queries", |db: Db, uri: Uri| async move {
            let n = tfb::query_count(uri.query());
            tfb::multiple_queries(&db, n)
                .await
                .map(Json)
                .map_err(Error::from)
        })
        .get("/updates", |db: Db, uri: Uri| async move {
            let n = tfb::query_count(uri.query());
            tfb::updates(&db, n).await.map(Json).map_err(Error::from)
        })
        .get("/fortunes", |db: Db| async move {
            tfb::fortunes(&db).await.map(Html).map_err(Error::from)
        })
    };
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
    app.run("0.0.0.0:3001")
}
