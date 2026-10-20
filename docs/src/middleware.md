# Middleware

Middleware is an async function that receives the request plus `next`, the rest of the pipeline:

```rust
use std::time::Instant;
use zapline::prelude::*;

async fn timing(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let mut res = next.run(req).await;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    res.headers_mut().insert("server-timing", HeaderValue::from_str(&format!("app;dur={ms:.2}")).unwrap());
    res
}

App::new().wrap(timing);
```

Middleware can short-circuit by returning a response without calling `next.run`. It can also modify the request (headers, extensions, URI) before passing it on, and modify the response afterwards. For configurable middleware, implement the `Middleware` trait on a struct.

## Where middleware runs

```
request ─▶ global (App::wrap) ─▶ routing ─▶ route (App::wrap_route) ─▶ group (Routes::wrap) ─▶ handler
```

| Kind | Registered with | Sees 404s? | `req.route()` known? | Typical use |
|---|---|---|---|---|
| Global | `App::wrap` | yes | no | CORS, request IDs, path normalization, tracing, compression, security headers |
| Route | `App::wrap_route` | no | yes | metrics (route labels), auth policies, per-route rate limits |
| Group | `Routes::wrap` | no | yes | auth for `/admin/*`, API-version-specific behavior |

Middleware runs in the order it was added (the first `wrap` is the outermost layer).

## Passing data to handlers

Insert typed values into the request extensions and read them with `Extension<T>`:

```rust
#[derive(Clone)]
struct Tenant(String);

async fn tenant(mut req: Request, next: Next) -> Response {
    let t = req.headers().get("x-tenant").and_then(|v| v.to_str().ok()).unwrap_or("default").to_owned();
    req.extensions_mut().insert(Tenant(t));
    next.run(req).await
}

async fn handler(Extension(Tenant(t)): Extension<Tenant>) -> String { t }
```

## Built-in middleware

| Middleware | Purpose |
|---|---|
| `Logger` | One line per request: method, path, route, status, latency |
| `Cors` | CORS with permissive or allow-list configuration and preflight handling |
| `Timeout` | Fail requests that exceed a duration with 503 |
| `DefaultHeaders` | Add headers to every response |
| `Compression` | gzip, br, zstd or deflate, negotiated with q-values (feature `compression`) |
| `RateLimit` | Per-IP, per-key, global and per-route limits with standard `RateLimit-*` headers |
| `SetRequestId` / `RequestId` | Generate and propagate `x-request-id` |
| `SecurityHeaders` | Hardened default response headers (HSTS, nosniff, frame options, CSP, ...) |
| `CatchPanic` | Turn handler panics into 500s |
| `ConcurrencyLimit` | Cap in-flight requests, with load shedding |
| `NormalizePath` | Trailing-slash and duplicate-slash normalization |
| `telemetry::Trace` | `tracing` spans and events per request |

See [Hardening middleware](hardening.md) for configuration details.
