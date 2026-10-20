use serde::Serialize;
use zapline::prelude::*;

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
    App::new()
        .get("/plaintext", || async { "Hello, World!" })
        .get("/json", || async {
            Json(Message {
                message: "Hello, World!",
            })
        })
        .get("/user/{id}", |Path(id): Path<u64>| async move {
            Json(User { id, name: "ada" })
        })
        .workers(workers)
        .run("127.0.0.1:3001")
}
