# Quickstart

> On **Windows**, install the Visual Studio C++ Build Tools first and use PowerShell. See [Platform support](platforms.md#setting-up-on-windows).

## With the CLI (recommended)

```sh
cargo install --path crates/zapline-cli     # installs `zapline` and `cargo zapline`
zapline new my-api                          # templates: minimal | api (default) | full
cd my-api
zapline dev                                 # build, run, live-reload the browser on every change
```

Open <http://localhost:8080/docs> for the interactive API docs, and run `cargo test` for the generated tests.

Or skip the project entirely: `zapline new hello.rs && zapline dev hello.rs` gives you the same in a
single file, dependencies declared at its top ([One-file apps](one-file.md)).

## By hand

```toml
[dependencies]
zapline = "0.1"

[dev-dependencies]
tokio = { version = "1", features = ["full"] }
```

```rust
use zapline::prelude::*;

// `#[model]` derives Debug, Clone, Serialize, Deserialize, JsonSchema and Validate.
#[model]
struct Todo {
    #[validate(length(min = 1, max = 200))]
    title: String,
    done: bool,
}

/// Create a todo.
#[post("/todos", tag = "todos", status = 201)]
async fn create(Valid(Json(todo)): Valid<Json<Todo>>) -> (StatusCode, Json<Todo>) {
    (StatusCode::CREATED, Json(todo))
}

/// Say hello.
#[get("/hello/{name}")]
async fn hello(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

pub fn app() -> App {
    App::new()
        .mount((create, hello))
        .openapi(OpenApi::new("Todos", "0.1.0"))
}

fn main() -> std::io::Result<()> {
    app().run("0.0.0.0:8080")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_empty_titles() {
        let res = app().test().post("/todos").json(&serde_json::json!({"title": "", "done": false})).send().await;
        assert_eq!(res.status(), 422);
    }
}
```

What you get:

- `POST /todos` parses and validates the body. Invalid input returns 422 with per-field `details`.
- `/openapi.json` and Swagger UI at `/docs` are generated from the signatures and doc comments.
- A thread-per-core server with graceful shutdown on SIGINT/SIGTERM.

Next, read [Routing](routing.md) and [Extractors](extractors.md), or browse the runnable examples with `cargo run -p zapline-examples --example <name>`.
