# Architecture

zapline has a small, fast **core** (routing, extractors, responses, errors, the server) and a set of **feature modules** built only on the core's public extension points. Everything lives in one crate, and each module that pulls in a dependency sits behind a Cargo feature (see [Feature flags](features.md)). This page is the short version. The full contract is in [`ARCHITECTURE.md`](https://github.com/your-org/zapline/blob/main/ARCHITECTURE.md) at the root of the repository, which is the document to read before contributing.

## Design rules

1. **No per-request cost for unused features.** If an app doesn't use a module, the hot path doesn't change. Work happens at build time (`on_build`, plugin installation) rather than per request.
2. **FastAPI-level ergonomics.** The common case is one line, errors are `zapline::Error`, and `?` works everywhere.
3. **Enterprise-grade control.** Every default can be overridden, through builders for configuration and traits for behavior (`RateLimitKey`, `SessionStore`, `HealthCheck`, `Authenticator`).
4. **No `unsafe` outside the core server and no panics on request paths.** Panics are reserved for invalid configuration at startup, such as a JWT secret that is too short.
5. **Tested.** Every module has integration tests, using the in-process test client where possible and a real socket where the feature needs one.

## A request, end to end

```text
TCP accept (one runtime per core, SO_REUSEPORT)
  -> HTTP connection (hyper)
  -> global middleware      App::wrap        before routing, sees 404s
  -> router                 radix tree, then a method slot; sets the matched route and its params
  -> route middleware       App::wrap_route, Routes::wrap        after routing
  -> handler                extractors -> your function -> IntoResponse
```

The consequences of that order show up throughout this guide: CORS, request IDs and path normalization are global; metrics, auth policies and per-route rate limits are route middleware, because they need the matched route. See [Middleware](middleware.md) and [Routing](routing.md).

## Extension points

Plugins such as [OpenAPI](openapi.md), [Metrics and Health](observability.md) and [Trace](observability.md#request-tracing-with-trace) use the same APIs as your code:

| To do this | Use |
|---|---|
| Bundle routes, middleware, state and hooks | `Plugin` and `App::plugin` (any `FnOnce(App) -> App` is a plugin) |
| Inspect every route after the table is final, and add routes | `App::on_build`. This is how `/openapi.json` is generated once at startup. |
| Initialize resources before accepting connections | `App::on_startup`, with `Resources` behind `State<T>` and [`Dep<T>`](di.md) |
| Attach typed data to routes | `Routes::meta`, `RouteInfo::meta::<T>()`. Auth policies, rate limits and OpenAPI docs all work this way. |
| Re-render every error | `App::error_handler` with `ErrorReport` |
| Write extractors and responses | `FromRequestParts`, `FromRequest`, `IntoResponse` |

## Where each module is documented

| Area | Modules | Chapter |
|---|---|---|
| Core | `app`, `router`, `request`, `response`, `extract`, `error`, `lifecycle`, `test` | [Routing](routing.md), [Extractors](extractors.md), [Responses](responses.md), [Errors](errors.md), [Lifecycle](lifecycle.md), [Testing](testing.md) |
| Server | `server`, `tls` | [Server](server.md) |
| Middleware | `middleware/*` | [Middleware](middleware.md), [Hardening middleware](hardening.md) |
| API docs and validation | `openapi/*`, `validate`, `zapline-macros` | [OpenAPI](openapi.md), [Validation](validation.md) |
| Application structure | `di`, `config`, `tasks` | [Dependency injection](di.md), [Configuration](config.md), [Background tasks](tasks.md) |
| Security | `auth/*`, `cookies`, `session` | [Authentication](auth.md), [Cookies, sessions & CSRF](sessions.md) |
| Realtime and content | `ws`, `sse`, `stream`, `fs`, `multipart` | [WebSockets](websockets.md), [Streaming](streaming.md), [Static files & uploads](files.md) |
| Observability | `telemetry`, `metrics`, `health` | [Observability](observability.md) |
| Tooling | `zapline-cli` | [Project layout & the CLI](cli.md) |

## Contributing a feature

Add it as a module in the `zapline` crate behind a feature if it needs a dependency, build it on the extension points above (never add fields to `Request` or work to the request path for a single feature), and give every public type a doc comment with a short example. Run `./scripts/check.sh` before opening a pull request, and include before and after `./bench/run.sh` numbers for hot-path changes. `ARCHITECTURE.md` and `CONTRIBUTING.md` have the details.
