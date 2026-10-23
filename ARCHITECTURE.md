# zapline architecture

zapline has a small, fast **core** (routing, extractors, responses, errors, server) and a set of **feature modules** built on public extension points. Every feature module is in the same crate, behind a Cargo feature where it pulls in dependencies. It's built *only* on the extension points below, so it costs nothing when unused.

## Design rules

1. **No per-request cost for unused features.** If an app doesn't use a module, the hot path must not change. Prefer build-time work (in `on_build`, plugin install) over per-request work. Never add fields to `Request` or work to `AppInner::handle` for a single feature.
2. **FastAPI-level ergonomics.** The common case is one line. Every public type has a doc comment with a short example. Errors are `zapline::Error`, and `?` works everywhere.
3. **Enterprise-grade control.** Every default can be overridden: builder methods for the config, traits for the behavior (e.g. `RateLimitKey`, `SessionStore`, `HealthCheck`).
4. **Zero `unsafe`** outside the core server, and no panics on request paths. Panics are only allowed on invalid *configuration* at startup.
5. **Tested.** Every module has integration tests under `crates/zapline/tests/`. Tests use the in-process `TestClient` where possible, and a real socket where the feature needs one.
6. **Mistakes have codes.** An error or startup panic caused by how the app is put together (a missing provider, a duplicate route, a feature that is off) gets a diagnostic code from `crates/zapline/src/codes.in` (`.diagnosed("ZL00xx", subject)` on the `Error`, `codes::with_help` in panic messages), whose fix is the code to add. Codes are never renumbered; add new ones at the end of their group, then run `./scripts/gen-llms.sh` and `ZAPLINE_BLESS=1 cargo test -p zapline-cli codes::` to refresh the CLI copy and the docs index.

## Request lifecycle

```
TCP accept (per-core runtime, SO_REUSEPORT)            server.rs
  → hyper HTTP/1.1 (or HTTP/2) connection
  → AppInner::handle(req, ConnInfo)                     app.rs
      → global middleware   (App::wrap)                  runs BEFORE routing
      → Router::route        radix tree → method slot    router.rs
          sets req.route = &'static RouteInfo, params
      → route middleware    (App::wrap_route, Routes::wrap)  AFTER routing
      → handler: extractors (FromRequestParts..., FromRequest) → IntoResponse
```

## Extension points (the contract)

| API | Purpose |
|---|---|
| `Plugin` trait, `App::plugin(p)`. Any `FnOnce(App) -> App` is a plugin. | Bundle routes, middleware, state and hooks. |
| `App::on_build(\|table: &[&'static RouteInfo]\| -> Routes)` | Runs once the route table is final. It sees every route plus its metadata, and can add routes. OpenAPI uses this. |
| `App::on_startup(\|res: Resources\| async { ...; Ok(()) })` | Async init before accepting connections (DB pools, config, DI). Values you `res.insert(v)` become `State<T>`. |
| `App::on_shutdown(\|res\| async {})` | Runs after the server stops, in reverse order. |
| `Resources` | The typed registry behind `State<T>`: `insert`, `insert_arc`, `get`, `get_arc`, `require`, `contains`. |
| `App::wrap(mw)` | Global middleware, before routing. Use it for CORS, request IDs, path normalization, and anything that must see 404s. |
| `App::wrap_route(mw)`, `Routes::wrap(mw)` | Middleware that runs after routing. `req.route()` and `req.matched_path()` are available. Use it for metrics, auth policies and per-route config. |
| `Routes::meta(v)`, `Routes::meta_all(v)`, `App::meta(v)`, `Routes::route_with(m, p, h, Extensions)`, `Routes::last_meta_mut()`, `Routes::defaults_mut()`, `Routes::map_meta_all(f)` | Typed per-route metadata. Values must be `Clone + Send + Sync + 'static`. Plugins define their own metadata types and read them via `RouteInfo::meta::<T>()`. Group defaults (`meta_all`) are replaced by inner groups' values; `map_meta_all` hooks compose instead (inner groups' run first, e.g. OpenAPI `tag_all`). |
| `Routes::endpoint(method, path, Arc<dyn Endpoint>)` | Register a dynamically built endpoint. |
| `App::extensions_mut()` | Builder-time typed storage for plugins (e.g. accumulated config). |
| `App::error_handler(\|r: &ErrorReport\| -> Response)` | Re-render every error response. Each error response carries an `ErrorReport` in its extensions. |
| `Error::with_code / with_details / with_header / with_source` | Structured errors. The default rendering is `{"error", "code"?, "details"?}`. |
| `Request::{route, matched_path, conn, remote_addr, head, head_mut, take_body, body_limit, extensions_mut, state}` | Request introspection for middleware and extractors. |
| `ConnInfo` (also an extractor), `MatchedPath` extractor | Connection and route info. |
| `FromRequestParts` / `FromRequest` | Custom extractors. Write them as `async fn` in the impl. |
| `IntoResponse` | Custom responses. |
| `ServerConfig` (`#[non_exhaustive]`, lives in `server.rs`) | Server tuning. Add new fields with defaults. |
| `ENV_PRINT_ROUTES` (`ZAPLINE_PRINT_ROUTES=1`) | `App::run` prints the route table as JSON and exits. Used by the CLI. |
| `ZAPLINE_EXPORT_OPENAPI=<path>` | The OpenAPI plugin writes the spec from its `on_build` hook and exits. Used by the CLI. |
| `ZAPLINE_DEV=1`, `App::dev_errors`, `ZAPLINE_DEV_ADDR_FILE=<path>` | Debug builds: the dev error page (installed as the outermost middleware in `App::prepare`, only when on), and the file where the server writes its URL for `zapline mcp`. Both set by `zapline dev`. |
| `zapline::codes` | The diagnostic code registry (`ALL`, `lookup`); `Error::diagnostic_code()`. |

Inherent `impl App { ... }` / `impl Routes { ... }` blocks may live in a feature module's own file. For example, `openapi/mod.rs` can add `App::openapi(...)`. That keeps each feature self-contained.

## Module map

| Module | Feature | Contents |
|---|---|---|
| `app`, `router`, `request`, `response`, `extract`, `handler`, `body`, `error`, `lifecycle`, `test` | core | The framework core |
| `server`, `tls` | core, `tls`, `http2` | Thread-per-core server, graceful shutdown, TLS, HTTP/2 |
| `middleware/*` | core, `compression` | Logger, CORS, Timeout, DefaultHeaders, compression, rate limiting, request IDs, security headers, catch-panic, concurrency limit, path normalization |
| `openapi/*`, `validate`, `__private` | `openapi`, `validate`, `macros` | OpenAPI 3.1 generation, docs UIs, `Valid<T>` |
| `ws`, `sse`, `fs`, `multipart` | `ws`, —, `fs`, `multipart` | Realtime and content |
| `auth/*`, `cookies`, `session` | core, `jwt`, `cookies` | Bearer/Basic/API-key/JWT auth, guards, cookies, sessions |
| `di`, `config`, `tasks` | core, `config` | Dependency injection, layered config, background tasks and jobs |
| `telemetry`, `metrics`, `health` | `tracing`, core | Tracing, Prometheus metrics, health checks |
| `zapline-macros` | `macros` | Route attribute macros |
| `zapline-cli` | — | `zapline new / generate / dev / routes / openapi / bench / doctor` |

## Testing and checks

```sh
cargo fmt --all
cargo clippy -p zapline --all-features --all-targets -- -D warnings
cargo test -p zapline --all-features
cargo check -p zapline --no-default-features
cargo build -p zapline-examples --examples
./bench/run.sh            # for hot-path changes
```
