# Introduction

**zapline** is a web framework for Rust that brings three things together:

- **FastAPI-level ergonomics.** You write plain `async fn`s with typed arguments, and you get request parsing, validation, errors, and interactive OpenAPI docs automatically.
- **Enterprise-grade structure.** It includes dependency injection, layered configuration, lifecycle hooks, plugins, guards and policies, observability, and health checks. These are the building blocks large teams expect from Spring Boot, NestJS or ASP.NET Core.
- **Top-tier performance.** Thread-per-core runtimes, zero-allocation routing, and no per-request cost for features you don't use. On equal hardware, zapline outperforms axum and actix-web in its own benchmark suite.

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

## Philosophy

1. **The common case is one line.** Each feature has a default that works out of the box: `App::openapi(...)`, `.plugin(Health::new())`, `.wrap(Cors::permissive())`.
2. **Everything is overridable.** Defaults are built from public traits and builders, so you can swap any piece.
3. **Pay only for what you use.** Optional features sit behind Cargo features and extension points. An app that doesn't use metrics runs exactly the same code path as if metrics didn't exist.
4. **Testable by default.** `app.test()` runs the whole pipeline in-process with no sockets.

## How this guide is organized

- *Getting started* gets an app running and explains the CLI.
- *Core concepts* covers the request pipeline: routing, extractors, responses, errors and middleware.
- *Building APIs*, *Security*, *Realtime & content* and *Operations* cover the batteries.
- The *Reference* lists every Cargo feature and explains how the framework is put together.
