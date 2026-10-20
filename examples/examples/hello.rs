//! The smallest zapline app.
//!
//!     cargo run --release --example hello
//!     curl localhost:8080/hello/world

use zapline::prelude::*;

#[get("/hello/{name}")]
async fn hello(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

fn main() -> std::io::Result<()> {
    App::new()
        .get("/", || async { "zapline is running" })
        .mount(hello)
        .run("0.0.0.0:8080")
}
