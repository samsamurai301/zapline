//! The actix-web equivalent of `zapline new --template minimal`.

use actix_web::{get, web};

/// `GET /`: a plain-text greeting.
#[get("/")]
async fn index() -> &'static str {
    "Hello from actix-minimal!"
}

/// `GET /hello/{name}`: greets `name`.
#[get("/hello/{name}")]
async fn hello(name: web::Path<String>) -> String {
    format!("Hello, {name}!")
}

/// Registers the routes.
pub fn app(cfg: &mut web::ServiceConfig) {
    cfg.service(index).service(hello);
}
