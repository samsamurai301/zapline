# zapline

**The productive, batteries-included Rust web framework: FastAPI ergonomics, enterprise structure, and top-of-the-chart performance.**

```rust
use serde::{Deserialize, Serialize};
use zapline::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema, Validate)]
struct NewUser {
    #[validate(length(min = 1, max = 64))]
    name: String,
    #[validate(email)]
    email: String,
}

/// Create a user.
#[post("/users", tag = "users", status = 201)]
async fn create_user(Valid(Json(user)): Valid<Json<NewUser>>) -> (StatusCode, Json<NewUser>) {
    (StatusCode::CREATED, Json(user))
}

fn main() -> std::io::Result<()> {
    App::new()
        .mount(create_user)
        .openapi(OpenApi::new("Users API", "1.0.0"))   // /openapi.json + Swagger UI at /docs
        .run("0.0.0.0:8080")
}
```

That's a complete service. The request body is parsed and validated, and invalid input gets a 422 with per-field details. The route is documented in OpenAPI 3.1 with interactive docs. A thread-per-core server serves it, with graceful shutdown.

## Why zapline

| | |
|---|---|
| ⚡ **Fast by construction** | Thread-per-core runtimes with `SO_REUSEPORT`, zero-allocation routing, and build-time work instead of per-request work. It beats axum and actix-web on its own [benchmark suite](#benchmarks). |
| 🧑‍💻 **FastAPI-style DX** | Typed extractors, automatic OpenAPI from handler signatures, validation with field-level errors, `?` everywhere, an in-process test client, and a CLI with project scaffolding and a reloading dev server. |
| 🏢 **Enterprise structure** | Dependency injection, layered config with profiles, lifecycle hooks, plugins, guards and policies, structured errors, health checks, Prometheus metrics and tracing. |
| 🔒 **Secure defaults** | Auth policies fail closed. It has constant-time credential checks, path-traversal-proof static files, signed/private cookies, CSRF protection, rate limiting, security headers and body limits. |
| 🧩 **Pay for what you use** | Optional features are Cargo features built on public extension points. Unused features cost nothing at runtime. |

## Features at a glance

| Area | What you get |
|---|---|
| Routing | `#[get]`/`#[post]`/... macros or builder methods, typed path params, nested groups, per-route metadata, 404/405 handling |
| Extractors | `Path`, `Query`, `Json`, `Form`, `Bytes`, `State`, `Dep`, `Valid`, `Multipart`, `BodyStream`, `CookieJar`, `Session`, `CurrentUser`, `Jwt`, `BearerToken`, `ConnInfo`, ... |
| API docs | OpenAPI 3.1 generated from signatures and doc comments; Swagger UI, Scalar and Redoc; security schemes from auth policies; export with `zapline openapi` |
| Validation | `Valid<Json<T>>` (also `Form`, `Query`, `Path`) using `validator` derives, returning `422` with `[{field, code, message}]` |
| App structure | `Dep<T>` DI with trait objects and async factories, `Config::load()` (TOML layers + env + `.env` + profiles), `on_startup`/`on_shutdown`, plugins |
| Background work | `BackgroundTasks` after the response, `App::schedule(Schedule::every(..))`, long-running workers with shutdown tokens |
| Security | Bearer, Basic and API-key extractors; JWT (HS/RS/ES); `Authenticate` + `CurrentUser`; `.require_roles/.require_scopes` guards; cookies; sessions; CSRF |
| Middleware | CORS, compression (br/zstd/gzip), rate limiting (per IP/key/route), request IDs, security headers, timeouts, catch-panic, load shedding, path normalization |
| Realtime & content | WebSockets, Server-Sent Events, streaming bodies and NDJSON, static files (ETag/Range/precompressed), multipart uploads |
| Observability | `tracing` spans per request, Prometheus `/metrics` with route labels, `/health/live` and `/health/ready` with checks |
| Server | HTTP/1.1 with pipelining, HTTP/2, TLS (rustls), graceful shutdown, connection limits, per-core or work-stealing runtime |
| Tooling | `zapline new` (minimal, api and full templates), `generate`, `dev` (auto-restart), `routes`, `openapi`, `doctor`, `bench` |

## Getting started

```sh
cargo install --path crates/zapline-cli      # provides `zapline` and `cargo zapline`
zapline new my-api                           # --template minimal | api | full
cd my-api && zapline dev                     # rebuilds and restarts on change
```

Or add it to an existing project:

```toml
[dependencies]
zapline = "0.1"                                  # default features: macros, json, query, form, openapi, validate, config, tracing
serde = { version = "1", features = ["derive"] }
schemars = "1"                                   # for #[derive(JsonSchema)]
validator = { version = "0.21", features = ["derive"] }
```

Enable more with features such as `ws`, `fs`, `multipart`, `compression`, `jwt`, `cookies`, `tls` and `http2`, or turn on everything with `full`.

## A tour

```rust
use std::sync::Arc;
use std::time::Duration;
use zapline::auth::{Authenticate, Claims, CurrentUser, JwtConfig};
use zapline::health::Health;
use zapline::metrics::Metrics;
use zapline::middleware::{Cors, RateLimit, SecurityHeaders, SetRequestId};
use zapline::prelude::*;
use zapline::tasks::Schedule;

trait UserRepo: Send + Sync {
    fn name(&self, id: u64) -> Option<String>;
}

/// Look up a user by id.
#[get("/users/{id}", tag = "users")]
async fn get_user(repo: Dep<dyn UserRepo>, Path(id): Path<u64>) -> Result<String> {
    repo.name(id).ok_or_else(|| Error::not_found().with_code("user_not_found"))
}

#[get("/me")]
async fn me(CurrentUser(claims): CurrentUser<Claims>) -> String {
    claims.sub
}

fn app(repo: Arc<dyn UserRepo>, jwt_secret: &str) -> App {
    let api = Routes::new()
        .mount((get_user, me))
        .delete("/users/{id}", || async { StatusCode::NO_CONTENT })
        .require_roles(["admin"])                             // guard: fails closed
        .wrap(Authenticate::jwt());                           // JWT for the whole group

    App::new()
        .provide_dyn::<dyn UserRepo>(repo)                     // dependency injection
        .jwt(JwtConfig::hs256(jwt_secret))
        .wrap(SetRequestId::new())
        .wrap(SecurityHeaders::default())
        .wrap(Cors::permissive())
        .wrap(RateLimit::per_ip(100, Duration::from_secs(60)))
        .nest("/api", api)
        .schedule(Schedule::every(Duration::from_secs(300)), |_res| async { /* cleanup */ })
        .plugin(Health::new())                                 // /health/live, /health/ready
        .plugin(Metrics::new())                                // /metrics
        .openapi(OpenApi::new("Tour", "1.0.0").bearer_auth("bearerAuth"))
}
```

Testing it needs no sockets:

```rust
#[tokio::test]
async fn unknown_user_is_404() {
    let client = app(Arc::new(MemRepo::default()), "secret").test();
    let token = JwtConfig::hs256("secret").encode(&Claims::new("ada")).unwrap();
    let res = client.get("/api/users/42").bearer(&token).send().await;
    assert_eq!(res.status(), 404);
}
```

## Documentation

- **The guide** (`docs/`, an mdBook): getting started, core concepts, API docs and validation, DI and config, security, realtime, observability, deployment, and performance. Build it with `mdbook serve docs`.
- **API reference**: `cargo doc -p zapline --all-features --open`
- **Examples** in [`examples/examples`](examples/examples): `hello`, `rest_api`, `openapi`, `enterprise_layered`, `auth_jwt`, `auth_session`, `middleware_stack`, `observability`, `realtime_chat`, `realtime_sse`, `realtime_files`. Run one with `cargo run -p zapline-examples --example openapi`.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) covers the internals and the plugin extension points.

## Benchmarks

The same three endpoints are implemented identically in zapline, axum 0.8, and actix-web 4 (all with mimalloc, release + fat LTO). See [`bench/`](bench/).

<!-- BENCH:START -->
| test | zapline | axum | actix-web |
|---|---:|---:|---:|
| plaintext | 277,311 req/s (741.13us) | 165,496 req/s (1.50ms) | 244,669 req/s (0.93ms) |
| json | 242,054 req/s (0.96ms) | 175,400 req/s (1.43ms) | 225,455 req/s (1.03ms) |
| path-param | 229,457 req/s (1.04ms) | 152,483 req/s (1.67ms) | 192,076 req/s (1.26ms) |
| plaintext-pipelined-16 | 1,672,525 req/s (1.42ms) | 92,298 req/s (24.46ms) | 1,215,920 req/s (1.91ms) |

_2 server workers on CPUs 0,1; wrk 2 threads / 256 connections on CPUs 2,3; best of 3 × 10s; 4 CPUs total, Linux 6.18.44-fc-v139._
<!-- BENCH:END -->

Reproduce with `./bench/run.sh` (needs `wrk`), and please run it on your own hardware. Numbers from a 4-vCPU VM where the client and server share the machine are only useful for *relative* comparison. axum's pipelined result reflects that `axum::serve` doesn't enable hyper's pipeline flushing by default.

## Project status

zapline is **0.1, pre-release**. The API may change before 1.0. It has about 830 tests, an adversarial security review whose findings were fixed, and a compile-checked guide. [ROADMAP.md](ROADMAP.md) lists deferred items and next steps. Planned next:

- [ ] WebSockets over HTTP/2 (RFC 8441), WebTransport
- [ ] Pluggable distributed stores for rate limiting and sessions (Redis)
- [ ] Full cron expressions for scheduled jobs
- [ ] OpenTelemetry OTLP exporter
- [ ] TechEmpower Framework Benchmarks submission (with DB tests)
- [ ] Database integration guides (sqlx, SeaORM, Diesel)

## Contributing

Contributions are very welcome. Run `./scripts/check.sh` before opening a PR; it runs fmt, clippy, the test suite, the feature matrix, the examples and the docs. Performance-sensitive changes should include before/after `./bench/run.sh` numbers. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
