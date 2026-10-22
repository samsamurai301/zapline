# Feature flags

zapline follows "pay only for what you use": anything that pulls in a dependency sits behind a Cargo feature, and a feature you don't use adds no code to your binary and no work to a request.

```toml
[dependencies]
zapline = "0.1"                                             # the default features
zapline = { version = "0.1", features = ["ws", "fs"] }      # default features plus WebSockets and static files
zapline = { version = "0.1", features = ["full"] }          # everything
zapline = { version = "0.1", default-features = false, features = ["json", "macros"] }   # a minimal build
```

## Every feature

The **default** features are `macros`, `json`, `query`, `form`, `openapi`, `validate`, `config` and `tracing`. The `full` feature enables the defaults plus everything else.

| Feature | Default | Enables | Implies | Dependencies it adds |
|---|---|---|---|---|
| `macros` | yes | The route attribute macros `#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]`, `#[head]`, `#[options]` and `#[route]`, with their OpenAPI arguments ([Routing](routing.md), [OpenAPI](openapi.md)) | | `zapline-macros` (`syn`, `quote`, `proc-macro2`) |
| `json` | yes | `Json<T>` as an extractor and a response, structured error `details`, `Ndjson`, `Sse::json_data`, `Message::json` and `Field::json` for WebSockets and uploads, and the test client's `.json()` | | `serde_json` |
| `query` | yes | The `Query<T>` extractor | | `serde_urlencoded` |
| `form` | yes | The `Form<T>` extractor and the test client's `.form()` | | `serde_urlencoded` |
| `openapi` | yes | `zapline::openapi`: the `OpenApi` plugin, Swagger UI, Scalar and Redoc, the builder methods and `JsonSchema` ([OpenAPI](openapi.md)) | `json` | `schemars` |
| `validate` | yes | `zapline::validate`: `Valid<T>` and the 422 error format ([Validation](validation.md)) | `json` | `validator` (with `derive`) |
| `config` | yes | `zapline::config`: `Config`, `Secret<T>` and `App::config` ([Configuration](config.md)) | | `toml` |
| `tracing` | yes | `zapline::telemetry`: `Telemetry`, `init()` and `Trace` ([Observability](observability.md)). With it, framework logs (failed requests, panics, jobs) go through `tracing`; without it they go to stderr. | | `tracing`, `tracing-subscriber` (`env-filter`, `fmt`, `json`) |
| `http2` | `full` | HTTP/2 support on the server ([Server](server.md)) | | `hyper/http2`, `hyper-util` (`http2`, `server-auto`) |
| `tls` | `full` | `zapline::tls`: TLS with rustls ([Server](server.md)) | | `tokio-rustls` (`ring`, `tls12`) |
| `ws` | `full` | `zapline::ws`: WebSockets ([WebSockets](websockets.md)) | | `tokio-tungstenite` (`handshake`) |
| `fs` | `full` | `zapline::fs`: `ServeDir`, `ServeFile` and `App::static_files` ([Static files](files.md)) | | `mime_guess`, `tokio-util` (`io`) |
| `multipart` | `full` | `zapline::multipart`: the `Multipart` extractor ([Uploads](files.md#uploads-with-multipart)) | | `multer` |
| `compression` | `full` | `middleware::Compression` and `CompressionLevel` ([Hardening](hardening.md#compression)) | | `async-compression` (gzip, brotli, zstd, deflate, zlib), `tokio-util` |
| `jwt` | `full` | JSON Web Tokens: `JwtConfig`, `Claims`, `Jwt<C>`, `JwtAuthenticator`, `Authenticate::jwt()` and `App::jwt` ([Authentication](auth.md#jwt)) | `json` | `jsonwebtoken` (RustCrypto backend, PEM keys) |
| `cookies` | `full` | `zapline::cookies` (`CookieJar`, signed and private jars, `Key`), `zapline::session` (`SessionLayer`, `Session`) and `auth::{Csrf, CsrfToken}` ([Cookies, sessions & CSRF](sessions.md)) | `json` | `cookie` (`percent-encode`, `signed`, `private`), `getrandom`, `hmac`, `sha2` (signed CSRF tokens) |
| `views` | `full` | `zapline::view`: `Views`, `App::views`, `HxRequest`, `Hx` ([Views & htmx](views.md)) | | `minijinja` (`loader`) |
| `postgres` | | `zapline::db` for Postgres: `Database`, `Db`, `DbCheck`, migrations ([Databases](database.md)) | | `sqlx` (`postgres`, `runtime-tokio`, `migrate`, rustls) |
| `sqlite` | | `zapline::db` for SQLite, including `Database::sqlite_memory()` for tests ([Databases](database.md)) | | `sqlx` (`sqlite`, `runtime-tokio`, `migrate`) |
| `full` | | Every feature above except the database backends (pick `postgres` and/or `sqlite`) | all | |

`openapi`, `validate`, `jwt` and `cookies` turn on `json` as well, because their error bodies, schemas and session data are JSON.

## Always included

Some modules have no feature flag because they add no dependencies:

| Module | What it provides |
|---|---|
| Core | Routing, `Path`, `State`, `Extension`, `Bytes` and `String` extractors, responses, errors, the test client, startup/shutdown hooks and plugins |
| `middleware` | `Logger`, `Cors`, `Timeout`, `DefaultHeaders`, `CatchPanic`, `ConcurrencyLimit`, `NormalizePath`, `RateLimit`, `SetRequestId`, `SecurityHeaders` |
| `auth` | `BearerToken`, `BasicAuth`, `ApiKey`, `Authenticate`, `Guard`, route policies (JWT and CSRF need their own features) |
| `di` | `Dep<T>`, `Cached<T>`, `Inject` and the `provide*` methods |
| `tasks` | `BackgroundTasks`, `App::schedule`, `App::background` |
| `sse`, `stream` | Server-Sent Events, `StreamBody` and `BodyStream` (`Ndjson` needs `json`) |
| `metrics`, `health` | The Prometheus `Metrics` plugin and the `Health` plugin |

## Choosing features

- **A typical JSON API**: the defaults. Add `jwt` and `compression` when you need them.
- **A web app with server-rendered pages**: `cookies` (sessions, CSRF) and `fs` (assets), plus `multipart` for uploads.
- **Realtime features**: `ws`. Server-Sent Events need no feature.
- **A slim microservice**: `default-features = false, features = ["json", "macros"]` and add `config` or `tracing` if you use them. Types and macros behind a missing feature are not compiled, so the compiler tells you when you reach for one.
- **Everything while exploring**: `full`. The examples in the repository (`cargo run -p zapline-examples --example openapi`) are built with it.

Features are additive and unify across your dependency tree, so one crate enabling `full` enables it for all. The same applies to the macros: `#[get(..)]` works without `openapi`, it just doesn't document anything.

Put `#[model]` on request and response types and `zapline` is the only dependency you need: it derives `Serialize`, `Deserialize`, `JsonSchema` (with `openapi`) and `Validate` (with `validate`) through zapline's own re-exports. Writing the derives yourself still works; then add the crates they expand to:

```toml
serde = { version = "1", features = ["derive"] }                    # #[derive(Serialize, Deserialize)]
schemars = "1"                                                     # #[derive(JsonSchema)]
validator = { version = "0.21", features = ["derive"] }            # #[derive(Validate)]
tokio = { version = "1", features = ["macros", "rt"] }             # #[tokio::test], in [dev-dependencies]
```

`cargo doc -p zapline --all-features --open` builds the API reference with every module visible. The reference on docs.rs is built the same way.
