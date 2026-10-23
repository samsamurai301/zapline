//! The axum equivalent of `zapline new --template minimal`.

use axum::extract::Path;
use axum::routing::get;
use axum::Router;

/// `GET /`: a plain-text greeting.
async fn index() -> &'static str {
    "Hello from axum-minimal!"
}

/// `GET /hello/{name}`: greets `name`.
async fn hello(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

/// Builds the application.
pub fn app() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/hello/{name}", get(hello))
}
