# zapline

[![CI](https://github.com/samsamurai301/zapline/actions/workflows/ci.yml/badge.svg)](https://github.com/samsamurai301/zapline/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/zapline.svg)](https://crates.io/crates/zapline)
[![docs.rs](https://img.shields.io/docsrs/zapline)](https://docs.rs/zapline)
[![guide](https://img.shields.io/badge/guide-mdBook-blue)](https://samsamurai301.github.io/zapline/)
[![license](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](#license)

**FastAPI ergonomics at actix-web speed, built for humans and AI coding agents.**

A batteries-included Rust web framework: typed handlers, OpenAPI generated from your code, validation, DI, auth, sessions, realtime, metrics, a thread-per-core server, and a CLI that scaffolds, generates and live-reloads. Small API surface, one obvious way to do each thing, and docs packaged so an AI assistant can learn the common 90% of the framework in about 2k tokens. The same API takes [about half the tokens](bench/tokens/results.md) to write in zapline as in axum or actix-web.

```rust
use zapline::prelude::*;

#[model] // serde + JSON Schema + validation, no extra crates
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
| ⚡ **Fast by construction** | Thread-per-core runtimes with `SO_REUSEPORT`, zero-allocation routing, and build-time work instead of per-request work. In the [benchmark suite](#benchmarks) it is 1.5× axum, on par with actix-web on trivial endpoints, and about 30% ahead of actix-web on path parameters and pipelining. |
| 🧑‍💻 **FastAPI-style DX** | Typed extractors, automatic OpenAPI from handler signatures, validation with field-level errors, `?` everywhere, an in-process test client that can fuzz every route from its own OpenAPI schemas (`app.test().fuzz()`), and a CLI with project scaffolding and a reloading dev server. |
| 🏢 **Enterprise structure** | Dependency injection, layered config with profiles, lifecycle hooks, plugins, guards and policies, structured errors, health checks, Prometheus metrics and tracing. |
| 🔒 **Secure defaults** | Auth policies fail closed. It has constant-time credential checks, path-traversal-proof static files, signed/private cookies, CSRF protection, rate limiting, security headers and body limits. |
| 🤖 **AI-native** | Every new project ships an `AGENTS.md`. The guide is published as [`llms.txt`](https://samsamurai301.github.io/zapline/llms.txt) plus a ~2k-token [cheatsheet](docs/src/cheatsheet.md) covering the common 90%. `zapline mcp` serves docs, generators, routes, `check` and `test` to Claude Code, Cursor and other MCP clients, plus live tools on the running dev app (`recent_errors`, `call_route`). Every framework mistake has a stable code (`ZL0002`) with the exact fix, on a dev error page with a copyable fix prompt and in `zapline explain`, and `zapline check` / `zapline test` answer in 68–79% fewer tokens than cargo ([token benchmark](bench/tokens/results.md)). |
| 🧩 **Pay for what you use** | Optional features are Cargo features built on public extension points. Unused features cost nothing at runtime. |

## Features at a glance

| Area | What you get |
|---|---|
| Routing | `#[get]`/`#[post]`/... macros or builder methods, typed path params, nested groups, per-route metadata, 404/405 handling |
| Extractors | `Path`, `Query`, `Json`, `Form`, `Bytes`, `State`, `Dep`, `Valid`, `Multipart`, `BodyStream`, `CookieJar`, `Session`, `CurrentUser`, `Jwt`, `BearerToken`, `ConnInfo`, ... |
| API docs | OpenAPI 3.1 generated from signatures and doc comments; Swagger UI, Scalar and Redoc; security schemes from auth policies; export with `zapline openapi` |
| Validation | `Valid<Json<T>>` (also `Form`, `Query`, `Path`) using `validator` rules (`#[model]` derives them), returning `422` with `[{field, code, message}]` |
| App structure | `Dep<T>` DI with trait objects and async factories, `Config::load()` (TOML layers + env + `.env` + profiles), `on_startup`/`on_shutdown`, plugins |
| Background work | `BackgroundTasks` after the response, `App::schedule(Schedule::every(..))`, long-running workers with shutdown tokens |
| Security | Bearer, Basic and API-key extractors; JWT (HS/RS/ES); social login (GitHub, Google, any OAuth 2 provider, PKCE); `Authenticate` + `CurrentUser`; `.require_roles/.require_scopes` guards; cookies; sessions; CSRF |
| Middleware | CORS, compression (br/zstd/gzip), rate limiting (per IP/key/route), request IDs, security headers, timeouts, catch-panic, load shedding, path normalization |
| Realtime & content | WebSockets, Server-Sent Events, streaming bodies and NDJSON, static files (ETag/Range/precompressed, content-hashed URLs cached forever), multipart uploads |
| Observability | `tracing` spans per request, exported to any OpenTelemetry backend over OTLP (set `OTEL_EXPORTER_OTLP_ENDPOINT`, no SDK); Prometheus `/metrics` with route labels; `/health/live` and `/health/ready` with checks |
| Server | HTTP/1.1 with pipelining, HTTP/2, TLS (rustls), graceful shutdown, connection limits, per-core or work-stealing runtime |
| Full-stack | SQL databases (Postgres, SQLite via sqlx) with migrations at startup and a `Db` extractor; MiniJinja views with hot reload and htmx fragments; in-memory test databases and one-line test data (`Factory::new("posts").create(&pool)` fills required columns and parent rows from the schema) plus seeds (`zapline db seed`); accounts in one command (`zapline g auth`: Argon2id, database sessions, password reset by email, GitHub/Google login, htmx pages); durable jobs with retries in the same database; email with a dev inbox; file storage on disk or S3 with expiring links; caching (stampede-free `get_or_set`, page caching, ETags); live updates pushed to every open page over SSE or WebSockets; shared cache, live updates, rate limits and sessions in Redis for running several instances (built-in client); translations (TOML per language, `Accept-Language`, plurals, `t()` in templates); an admin panel at `/admin` for every table, read from the schema (search, sort, edit, no code per table); a dev console at `/_dev` (routes, recent requests, job queue, inbox); a typed TypeScript client generated from your handlers (`zapline client`, kept live by `zapline dev --client`) |
| Tooling | `zapline new` (minimal, api, full and fullstack templates, or `hello.rs` for a [one-file app](docs/src/one-file.md) run by `zapline run`/`dev`), `generate resource name field:type...` (migration + CRUD + page + tests), `db new/migrate`, `dev` (auto-restart, detailed errors), `g tailwind` (Tailwind CSS with no Node, rebuilt live by `dev`), `g deploy fly\|railway\|render\|compose`, `routes`, `openapi`, `doctor`, `bench` |

## Getting started

A full-stack app (HTML + htmx, SQL database, JSON API, tests) in four commands, with no database server and no Node:

```sh
cargo install zapline-cli                                        # provides `zapline` and `cargo zapline`
zapline new shop --template fullstack && cd shop                 # SQLite file, migrations, htmx pages, OpenAPI
zapline g resource product name price:float stock:int featured:bool   # migration + SQL CRUD API + htmx page + tests
zapline dev                                                      # http://localhost:8080/products and /docs
```

Add accounts with password reset (`zapline g auth`) and a durable background job (`zapline g job send_welcome`), and you have the [**saas example**](examples/saas): a multi-user app with sign-up, emailed reset links, per-user data, a welcome email sent by a retried job, a dev inbox at `/_dev/mail`, an admin panel for the accounts in `ADMIN_EMAILS`, and 13 end-to-end tests, about 100 lines beyond what the CLI generated.

Or an API service: `zapline new my-api` (templates: `minimal`, `api`, `full`, `fullstack`).

### A full app in one file

No `Cargo.toml`, no project directory: one `.rs` file with its dependencies at the top.

```rust
#!/usr/bin/env -S zapline run
//! ```cargo
//! [dependencies]
//! uuid = { version = "1", features = ["v4"] }   # zapline, serde, serde_json and tokio are built in
//! ```
use zapline::prelude::*;

#[get("/ids/new")]
async fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn main() -> std::io::Result<()> {
    App::new().mount(new_id).run("0.0.0.0:8080")
}
```

```sh
zapline run ids.rs        # or ./ids.rs; `zapline dev ids.rs` reloads on save, `zapline test ids.rs` runs its tests
```

Every one-file app shares one compiled dependency cache, and an unchanged file starts in milliseconds without running cargo. Errors in the header or the code point at your file and line. `zapline new hello.rs` writes a starter. See [One-file apps](docs/src/one-file.md).

zapline supports **Linux, macOS and Windows**: CI runs the full test suite on all three. On Windows, install the Visual Studio C++ Build Tools, then use PowerShell (`.\scripts\check.ps1`, `cargo install zapline-cli`). See [Platform support](docs/src/platforms.md) for the details.

Or add it to an existing project:

```toml
[dependencies]
zapline = "0.1"   # default features: macros, json, query, form, openapi, validate, config, tracing
```

`#[model]` derives serde, `JsonSchema` and `Validate` through zapline's re-exports, so no other crate is needed.

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

- **[Cheatsheet](docs/src/cheatsheet.md)**: one runnable file with the canonical way to do each common task. Start here, or point your AI assistant at it.
- **For AI assistants**: [`llms.txt`](https://samsamurai301.github.io/zapline/llms.txt) (index) and [`llms-full.txt`](https://samsamurai301.github.io/zapline/llms-full.txt) (the whole guide in one file). Agents working on zapline itself should read [`AGENTS.md`](AGENTS.md).
- **[The guide](https://samsamurai301.github.io/zapline/)** (`docs/`, an mdBook): getting started, core concepts, API docs and validation, DI and config, security, realtime, observability, deployment, and performance. Build it with `mdbook serve docs`.
- **API reference**: `cargo doc -p zapline --all-features --open`
- **The saas example** in [`examples/saas`](examples/saas): accounts, password reset, per-user data, durable jobs, email and a schedule in one app.
- **Examples** in [`examples/examples`](examples/examples): `cheatsheet`, `hello`, `rest_api`, `openapi`, `enterprise_layered`, `auth_jwt`, `auth_session`, `middleware_stack`, `observability`, `realtime_chat`, `realtime_sse`, `realtime_files`. Run one with `cargo run -p zapline-examples --example openapi`.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) covers the internals and the plugin extension points.

## Benchmarks

The same three endpoints are implemented identically in zapline, axum 0.8, and actix-web 4 (all with mimalloc, release + fat LTO). See [`bench/`](bench/).

<!-- BENCH:START -->
| test | zapline | axum | actix-web |
|---|---:|---:|---:|
| plaintext | 238393 req/s (0.93ms) | 158897 req/s (1.59ms) | 232987 req/s (0.99ms) |
| json | 230154 req/s (1.00ms) | 142491 req/s (1.79ms) | 226023 req/s (1.04ms) |
| path-param | 238642 req/s (0.98ms) | 153838 req/s (1.76ms) | 179485 req/s (1.40ms) |
| plaintext-pipelined-16 | 1597907 req/s (1.47ms) | 92241 req/s (24.50ms) | 1221584 req/s (1.91ms) |

_2 server workers on CPUs 0,1; wrk 2 threads / 256 connections on CPUs 2,3; best of 3 × 10s; 4 CPUs total, Linux 6.18.44-fc-v139._
<!-- BENCH:END -->

These numbers come from the complete framework with default features (OpenAPI, validation, tracing and so on compiled in). Features you don't use add no per-request cost. Hot-path changes are verified by instruction counts per request (callgrind), not only wall-clock time. Reproduce with `./bench/run.sh` (needs `wrk`), and please run it on your own hardware. Numbers from a 4-vCPU VM where the client and server share the machine are only useful for *relative* comparison. axum's pipelined result reflects that `axum::serve` doesn't enable hyper's pipeline flushing by default.

## Project status

zapline is **0.1, pre-release**. The API may change before 1.0. It has about 830 tests, an adversarial security review whose findings were fixed, and a compile-checked guide. [ROADMAP.md](ROADMAP.md) lists deferred items and next steps. Planned next:

- [ ] WebSockets over HTTP/2 (RFC 8441), WebTransport
- [ ] Pluggable distributed stores for rate limiting and sessions (Redis)
- [ ] Full cron expressions for scheduled jobs
- [ ] OpenTelemetry OTLP exporter
- [ ] TechEmpower Framework Benchmarks submission (with DB tests)
- [ ] Database integration guides (sqlx, SeaORM, Diesel)

## Contributing

Contributions are very welcome. Run `./scripts/check.sh` (or `.\scripts\check.ps1` on Windows) before opening a PR; it runs fmt, clippy, the test suite, the feature matrix, the examples and the docs. Performance-sensitive changes should include before/after `./bench/run.sh` numbers. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
