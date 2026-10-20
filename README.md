# zapline

**A fast, low-boilerplate web framework for Rust.** Write plain `async fn`s; zapline handles the routing, parsing, errors, and the server.

```rust
use zapline::prelude::*;

#[get("/hello/{name}")]
async fn hello(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

fn main() -> std::io::Result<()> {
    App::new().mount(hello).run("0.0.0.0:8080")
}
```

There's no `#[tokio::main]`, no router/service/layer types to wire up, and no listener boilerplate. `run` starts a thread-per-core server sized to your CPUs.

## Why zapline

| | |
|---|---|
| **Fast by construction** | Thread-per-core runtimes with `SO_REUSEPORT`, zero-allocation route params, fixed-slot method dispatch, no per-request work for features you don't use. See [benchmarks](#benchmarks). |
| **Little boilerplate** | Route macros, typed extractors, one `Error` type that `?` works with, state without generics, and a prelude. |
| **Flexible** | Plain closures work as handlers too. Middleware is just an `async fn(Request, Next)`. Route groups with their own middleware, custom extractors, custom fallback, or bring your own Tokio runtime with `App::serve`. |
| **Testable** | `app.test()` gives an in-process client that runs the full pipeline with no sockets. |
| **Open source** | MIT OR Apache-2.0. |

## A real-ish API

```rust
use std::sync::{Arc, RwLock};
use serde::{Deserialize, Serialize};
use zapline::middleware::{Cors, Logger};
use zapline::prelude::*;

#[derive(Clone, Serialize)]
struct Todo { id: u64, title: String, done: bool }

#[derive(Deserialize)]
struct NewTodo { title: String }

#[derive(Clone, Default)]
struct Db(Arc<RwLock<Vec<Todo>>>);

#[get("/todos/{id}")]
async fn get_todo(State(db): State<Db>, Path(id): Path<u64>) -> Option<Json<Todo>> {
    db.0.read().unwrap().iter().find(|t| t.id == id).cloned().map(Json)   // None -> 404
}

#[post("/todos")]
async fn create(State(db): State<Db>, Json(new): Json<NewTodo>) -> Result<(StatusCode, Json<Todo>)> {
    if new.title.is_empty() {
        return Err(Error::unprocessable("title must not be empty"));       // -> 422 JSON error
    }
    let mut db = db.0.write().unwrap();
    let todo = Todo { id: db.len() as u64 + 1, title: new.title, done: false };
    db.push(todo.clone());
    Ok((StatusCode::CREATED, Json(todo)))
}

async fn auth(req: Request, next: Next) -> Response {
    match req.headers().get("authorization") {
        Some(_) => next.run(req).await,
        None => Error::unauthorized().into_response(),
    }
}

fn main() -> std::io::Result<()> {
    let api = Routes::new().mount((get_todo, create)).wrap(auth);   // group middleware

    App::new()
        .state(Db::default())
        .wrap(Logger)                                              // global middleware
        .wrap(Cors::permissive())
        .get("/health", || async { "ok" })
        .nest("/api", api)
        .run("0.0.0.0:8080")
}
```

The full version is in [`examples/rest_api.rs`](examples/rest_api.rs).

## Guide

### Handlers
Any `async fn` (or closure returning a future) with up to 12 arguments. Every argument except the last must be a `FromRequestParts` extractor. The last one may consume the body.

### Extractors

| Extractor | What it gives you |
|---|---|
| `Path<T>` | Route params: `Path<u64>`, `Path<(String, u32)>`, or a `Deserialize` struct. Percent-decoded. |
| `Query<T>` | Query string via serde |
| `Json<T>` | JSON body (checks `Content-Type`, enforces the body limit) |
| `Form<T>` | URL-encoded form body |
| `Bytes`, `String` | Raw body |
| `State<T>` | App state registered with `.state(value)`. Register as many types as you like. |
| `Extension<T>` | A value inserted by middleware |
| `Method`, `Uri`, `HeaderMap`, `Request` | Request metadata or the whole request |
| `Option<T>` / `Result<T, Error>` | Make any extractor optional, or handle its error yourself |

To write your own, implement `FromRequestParts`:

```rust
struct ApiKey(String);

impl zapline::extract::FromRequestParts for ApiKey {
    async fn from_request_parts(req: &mut Request) -> Result<Self> {
        req.headers().get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|v| ApiKey(v.to_owned()))
            .ok_or_else(Error::unauthorized)
    }
}
```

### Responses
Return anything that implements `IntoResponse`: `&str`, `String`, `Bytes`, `Json<T>`, `Html<T>`, `Redirect`, `StatusCode`, `Option<T>` (where `None` means 404), `Result<T, E>`, `(StatusCode, T)`, `([(header, value); N], T)`, or a raw `Response`.

### Errors
`zapline::Result<T>` uses `zapline::Error`. Any `std::error::Error` converts with `?` into a `500`. The details are logged and **not** sent to the client. For client errors, use the constructors: `Error::bad_request(msg)`, `not_found()`, `unauthorized()`, `conflict(msg)`, `unprocessable(msg)`, or `Error::new(status, msg)`. Errors are rendered as `{"error": "..."}`.

### Route macros
`#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]`, `#[head]`, `#[options]`, and `#[route("/p", method = "GET", method = "POST")]`. The annotated function becomes a zero-sized value you pass to `.mount(...)`. Pass a tuple to mount several at once. If you'd rather skip the macros, `.get("/path", handler)` works with plain functions.

### Middleware
- **Global:** `App::wrap`. Runs before routing, for every request including 404s.
- **Group:** `Routes::wrap`. Runs after routing, only for that group.
- **Built-in:** `Logger`, `Cors`, `Timeout`, `DefaultHeaders`.

### Testing
```rust
#[tokio::test]
async fn creates_todo() {
    let client = app().test();
    let res = client.post("/api/todos").json(&serde_json::json!({"title": "x"})).send().await;
    assert_eq!(res.status(), 201);
}
```

### Server
`App::run(addr)` blocks and serves on N threads (CPU count by default, or `.workers(n)`). Each thread has its own single-threaded Tokio runtime and its own `SO_REUSEPORT` listener on Linux and FreeBSD. On other platforms it uses a multi-threaded runtime. To use your own runtime, call `App::serve(listener).await` instead. HTTP/1.1 keep-alive and pipelining are on. A 30 s header-read timeout (slowloris protection) is on by default. You can tune or disable it through `ServerConfig`.

## Benchmarks

The same three endpoints are implemented identically in zapline, axum 0.8, and actix-web 4 (all using mimalloc, release + fat LTO). See [`bench/`](bench/).

<!-- BENCH:START -->
| test | zapline | axum | actix-web |
|---|---:|---:|---:|
| plaintext | 277311 req/s (741.13us) | 165496 req/s (1.50ms) | 244669 req/s (0.93ms) |
| json | 242054 req/s (0.96ms) | 175400 req/s (1.43ms) | 225455 req/s (1.03ms) |
| path-param | 229457 req/s (1.04ms) | 152483 req/s (1.67ms) | 192076 req/s (1.26ms) |
| plaintext-pipelined-16 | 1672525 req/s (1.42ms) | 92298 req/s (24.46ms) | 1215920 req/s (1.91ms) |

_2 server workers on CPUs 0,1; wrk 2 threads / 256 connections on CPUs 2,3; best of 3 × 10s; 4 CPUs total, Linux 6.18.44-fc-v139._
<!-- BENCH:END -->

Reproduce with `./bench/run.sh` (needs `wrk`). Please run it on your own hardware. Numbers from a 4-vCPU VM where the client and server share one machine are useful for *relative* comparison only. They aren't comparable to TechEmpower's dedicated hardware.

### Where the performance comes from
1. **Thread-per-core + `SO_REUSEPORT`.** The kernel load-balances connections across per-core listeners. Connections never migrate, and there's no work-stealing scheduler on the hot path.
2. **Core-local refcounts.** Each worker gets its own `Arc` handles to shared app data, so per-request clones don't bounce cache lines between cores.
3. **Zero-alloc routing.** A radix tree (`matchit`), then a fixed-field method table. Route params are stored as byte ranges into the request path, inline for up to 4 params.
4. **One dynamic dispatch, one allocation** per routed request (the handler future). 404/405 responses allocate no future.
5. **Pipelining done right.** Responses to pipelined requests are flushed together (`pipeline_flush`).
6. **Pay for what you use.** No middleware means no middleware machinery runs. Extractors you don't declare do no work.

## Roadmap
Contributions are welcome. These are good first big items:
- [ ] HTTP/2 and TLS (rustls) behind features
- [ ] Graceful shutdown
- [ ] WebSockets and SSE helpers
- [ ] Static file serving
- [ ] Compression middleware
- [ ] `tracing` integration and an OpenAPI generator from route macros
- [ ] TechEmpower Framework Benchmarks submission (with DB tests)

## License
Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
