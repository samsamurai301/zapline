use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

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

fn main() {
    let workers: usize = std::env::var("WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
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
            let listener = tokio::net::TcpListener::bind("127.0.0.1:3002")
                .await
                .unwrap();
            axum::serve(listener, app).await.unwrap();
        });
}
