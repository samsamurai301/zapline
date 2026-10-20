use actix_web::{web, App, HttpServer};
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

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let workers: usize = std::env::var("WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    HttpServer::new(|| {
        App::new()
            .route("/plaintext", web::get().to(|| async { "Hello, World!" }))
            .route(
                "/json",
                web::get().to(|| async {
                    web::Json(Message {
                        message: "Hello, World!",
                    })
                }),
            )
            .route(
                "/user/{id}",
                web::get().to(|id: web::Path<u64>| async move {
                    web::Json(User {
                        id: id.into_inner(),
                        name: "ada",
                    })
                }),
            )
    })
    .workers(workers)
    .backlog(4096)
    .bind("127.0.0.1:3003")?
    .run()
    .await
}
