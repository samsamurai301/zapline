use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use bench_servers as tfb;
use serde::Serialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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

fn db_error(e: sqlx::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(tfb::workers())
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let app = Router::new()
                .route("/plaintext", get(|| async { "Hello, World!" }))
                .route(
                    "/json",
                    get(|| async {
                        Json(Message {
                            message: "Hello, World!",
                        })
                    }),
                )
                .route(
                    "/user/{id}",
                    get(|Path(id): Path<u64>| async move { Json(User { id, name: "ada" }) }),
                );
            // NO_DB=1 serves only the endpoints above (no Postgres needed).
            let app = if std::env::var("NO_DB").is_ok_and(|v| v == "1") {
                app
            } else {
                let pool = PgPoolOptions::new()
                    .max_connections(tfb::pool_size())
                    .connect(&tfb::database_url())
                    .await
                    .expect("connect to Postgres");
                app.merge(
                    Router::new()
                        .route(
                            "/db",
                            get(|State(pool): State<PgPool>| async move {
                                tfb::single_query(&pool).await.map(Json).map_err(db_error)
                            }),
                        )
                        .route(
                            "/queries",
                            get(
                                |State(pool): State<PgPool>, RawQuery(q): RawQuery| async move {
                                    let n = tfb::query_count(q.as_deref());
                                    tfb::multiple_queries(&pool, n)
                                        .await
                                        .map(Json)
                                        .map_err(db_error)
                                },
                            ),
                        )
                        .route(
                            "/updates",
                            get(
                                |State(pool): State<PgPool>, RawQuery(q): RawQuery| async move {
                                    let n = tfb::query_count(q.as_deref());
                                    tfb::updates(&pool, n).await.map(Json).map_err(db_error)
                                },
                            ),
                        )
                        .route(
                            "/fortunes",
                            get(|State(pool): State<PgPool>| async move {
                                tfb::fortunes(&pool).await.map(Html).map_err(db_error)
                            }),
                        )
                        .with_state(pool),
                )
            };
            let listener = tokio::net::TcpListener::bind("0.0.0.0:3002").await.unwrap();
            axum::serve(listener, app).await.unwrap();
        });
}
