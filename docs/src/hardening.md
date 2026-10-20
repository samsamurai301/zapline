# Hardening middleware

A handful of built-in middleware turn a working service into one you can put on the internet. Each is one line:

```rust
use std::time::Duration;
use zapline::middleware::{CatchPanic, RateLimit, SecurityHeaders, Timeout};
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .wrap(CatchPanic::new())
        .wrap(SecurityHeaders::default())
        .wrap(Timeout::new(Duration::from_secs(30)))
        .wrap_route(RateLimit::per_ip(300, Duration::from_secs(60)))
        .get("/", || async { "hello" })
}
```

| Middleware | Protects against | Feature |
|---|---|---|
| [`SecurityHeaders`](#securityheaders) | Clickjacking, MIME sniffing, downgrade attacks, over-broad browser features | |
| [`RateLimit`](#ratelimit) | Brute force, scraping, noisy clients | |
| [`Compression`](#compression) | Bandwidth cost (not an attack, but it is why you want it) | `compression` |
| [`ConcurrencyLimit`](#concurrencylimit) | Overload: sheds load instead of collapsing | |
| [`Timeout`](#timeout) | Handlers (or dependencies) that hang | |
| [`CatchPanic`](#catchpanic) | A bug in one handler dropping connections | |
| [`NormalizePath`](#normalizepath) | `/users` vs `/users/` vs `//users` inconsistencies | |
| [`SetRequestId`](#setrequestid-and-requestid) | Unattributable errors | |
| [`Cors`](#cors) | Cross-origin access you didn't intend | |
| `DefaultHeaders` | Adds fixed headers (`Server`, `Cache-Control`, ...) to every response. Headers the handler set win. | |

All of them are in `zapline::middleware`. Remember how middleware is arranged (see [Middleware](middleware.md)): `App::wrap` runs **before routing** for every request including 404s, and the first `wrap` is the outermost layer. `App::wrap_route` and `Routes::wrap` run **after routing** and can see the matched route and its metadata. The [last section](#a-production-stack) puts it all together.

## `SecurityHeaders`

Adds browser hardening headers to every response. Headers the handler already set are never overwritten, and the values are built once, at construction.

```rust
use std::time::Duration;
use zapline::middleware::SecurityHeaders;
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .wrap(SecurityHeaders::default())          // sensible defaults
}

fn https_only_app() -> App {
    App::new().wrap(SecurityHeaders::strict())     // + HSTS, CSP, CORP
}

fn custom() -> SecurityHeaders {
    SecurityHeaders::default()
        .hsts(Duration::from_secs(31_536_000), true, false)
        .csp("default-src 'self'; img-src 'self' https://cdn.example.com")
        .frame_options("SAMEORIGIN")
        .remove("x-dns-prefetch-control")
}
```

`SecurityHeaders::default()` sets:

| Header | Value |
|---|---|
| `X-Content-Type-Options` | `nosniff` |
| `X-Frame-Options` | `DENY` |
| `Referrer-Policy` | `strict-origin-when-cross-origin` |
| `Cross-Origin-Opener-Policy` | `same-origin` |
| `X-DNS-Prefetch-Control` | `off` |
| `Permissions-Policy` | camera, microphone, geolocation, payment, usb, ... all disabled |

It sets no `Content-Security-Policy`, so pages that load assets from a CDN (including the [interactive API docs](openapi.md)) keep working.

`SecurityHeaders::strict()` adds `Strict-Transport-Security: max-age=63072000; includeSubDomains; preload`, `Content-Security-Policy: default-src 'self'; base-uri 'self'; object-src 'none'; frame-ancestors 'none'; form-action 'self'; img-src 'self' data:; upgrade-insecure-requests` and `Cross-Origin-Resource-Policy: same-origin`. Think before you use it:

- **HSTS** makes browsers refuse plain HTTP for your whole domain (and, with `includeSubDomains`, its subdomains) for two years. Use it only when everything is served over HTTPS. Browsers ignore it over plain HTTP.
- **The strict CSP blocks the Swagger UI, Scalar and Redoc pages**, which load scripts from a CDN and use inline scripts. Either serve docs only in non-production, or relax the policy for your app: `SecurityHeaders::strict().csp("...")`, or `.remove("content-security-policy")`.

| Method | Effect |
|---|---|
| `.hsts(max_age, include_subdomains, preload)` | `Strict-Transport-Security` |
| `.csp(policy)` / `.csp_report_only(policy)` | `Content-Security-Policy` / `...-Report-Only`, to trial a policy |
| `.frame_options(value)` | `X-Frame-Options` (`DENY` or `SAMEORIGIN`) |
| `.referrer_policy(value)` | `Referrer-Policy` |
| `.permissions_policy(value)` | `Permissions-Policy`, for example `camera=(), geolocation=(self)` |
| `.header(name, value)` | Sets or replaces any header. Panics on an invalid name or value. |
| `.remove(name)` | Stops sending a header (case-insensitive) |

## `RateLimit`

Limits requests per client with GCRA, a smooth token bucket: `per_ip(100, 1 minute)` allows a burst of 100, then one request every 0.6 seconds. State lives in a sharded, bounded in-memory store (default 100,000 clients, about 2 MB). Expired entries are swept lazily, so there is no background task. Clones share state.

Limited requests get `429 Too Many Requests` with the code `rate_limited` and a `Retry-After` header:

```text
HTTP/1.1 429 Too Many Requests
content-type: application/json
retry-after: 30
ratelimit-limit: 2
ratelimit-remaining: 0
ratelimit-reset: 60

{"error":"too many requests","code":"rate_limited"}
```

Every response from a limited route carries `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset`. With several limiters stacked, the tightest one is reported. Turn the headers off with `.headers(false)`; a `429` always carries `Retry-After`.

### Who is the client?

| Constructor | Counts requests per |
|---|---|
| `RateLimit::per_ip(limit, period)` | Client IP address. IPv6 addresses are grouped by `/64` (see `.ipv6_prefix(bits)`), because one host usually controls a whole `/64`. |
| `RateLimit::global(limit, period)` | Everyone together (a lock-free single bucket) |
| `RateLimit::by_header(name, limit, period)` | Value of a header such as an API key. Requests without the header are limited per IP. |
| `RateLimit::by_key(limit, period, \|req\| Option<String>)` | Whatever your closure returns. `None` skips limiting for that request. |
| `RateLimit::keyed(limit, period, key)` | The same, with any `RateLimitKey` implementation |

A header-based key is only as trustworthy as the header. If clients can send any string as `x-api-key`, they can dodge the limit by sending a new one on every request. Key on identities you have **verified** (a user ID from your authenticator, a validated API key) and keep a per-IP limiter as well.

```rust
use std::time::Duration;
use zapline::middleware::RateLimit;
use zapline::prelude::*;

const MINUTE: Duration = Duration::from_secs(60);

fn limiters() -> (RateLimit, RateLimit) {
    let per_ip = RateLimit::per_ip(100, MINUTE)
        .burst(20)                                              // at most 20 back-to-back
        .trust_forwarded_for(["10.0.0.0/8"])
        .exempt(|req: &Request| req.path() == "/healthz");

    let per_tenant = RateLimit::by_key(1000, MINUTE, |req: &Request| {
        req.headers().get("x-tenant")?.to_str().ok().map(str::to_owned)
    });
    (per_ip, per_tenant)
}
```

| Method | Effect |
|---|---|
| `.burst(n)` | Allow up to `n` requests back to back (default: `limit`) |
| `.trust_forwarded_for(proxies)` | Take the client IP from `X-Forwarded-For`, but only when the connection comes from one of these proxies (see below) |
| `.trusted_proxies(TrustedProxies)` | Full control: the header (`X-Forwarded-For`, RFC 7239 `Forwarded`, or a single-address header such as `X-Real-IP`) |
| `.ipv6_prefix(bits)` | Group IPv6 clients by this prefix length (default 64) |
| `.exempt(\|req\| bool)` | Never limit requests for which this returns `true` |
| `.headers(bool)` | Send the `RateLimit-*` headers (default `true`) |
| `.max_keys(n)` | Maximum number of tracked clients (default 100,000). When full, the bucket closest to expiry is evicted. |

### Behind a proxy: trusted proxies

Behind a load balancer or reverse proxy, the TCP peer is always the proxy. If you do nothing, **every client shares one bucket** (the proxy's IP) and one busy client locks everyone out. The fix looks easy: read `X-Forwarded-For`. But that header is just text the client can send too, so trusting it blindly lets any client pick its own identity and dodge the limit, or frame someone else.

That is why forwarding headers are ignored unless the peer is a **trusted proxy** that you listed. The header is then read **right to left**, skipping trusted proxies, and the first untrusted address is the client. Addresses to the left of that (the part a client could have written) are never reached unless every hop is trusted.

```rust
use std::time::Duration;
use zapline::middleware::rate_limit::{ClientIpHeader, TrustedProxies};
use zapline::middleware::RateLimit;

fn limiter() -> RateLimit {
    let proxies = TrustedProxies::new(["10.0.0.0/8", "127.0.0.1"]).header(ClientIpHeader::Forwarded);
    RateLimit::per_ip(100, Duration::from_secs(60)).trusted_proxies(proxies)
}
```

List only networks you control, the exact addresses of your ingress. `limiter.client_ip(&req)` shows the address a limiter derives for a request.

### Per-route limits

A limiter installed with `App::wrap_route` or `Routes::wrap` runs after routing, so it can see the route's metadata. Its own limit is the default, and annotated routes get their **own limit and buckets**:

```rust
use std::time::Duration;
use zapline::middleware::{RateLimit, RateLimitPolicy};
use zapline::prelude::*;

const MINUTE: Duration = Duration::from_secs(60);

fn app() -> App {
    App::new()
        .post("/login", || async { "welcome" })
        .rate_limit(5, MINUTE)                          // 5 attempts per minute per client
        .post("/password-reset", || async { "sent" })
        .meta(RateLimitPolicy::new(3, MINUTE).burst(1).scope("auth"))   // shared bucket "auth"
        .get("/healthz", || async { "ok" })
        .meta(RateLimitPolicy::exempt())                // never limited
        .get("/items", || async { "items" })
        .wrap_route(RateLimit::per_ip(300, MINUTE))     // everything else
}
```

`.rate_limit(limit, period)` is on both `App` and `Routes` and applies to the most recently registered route. Policies that share a `.scope(name)` share buckets, for example to cap login and password reset together. A limiter installed with plain `App::wrap` runs before routing and cannot see policies: it applies its own limit to everything, 404s included.

## `Compression`

Needs the Cargo feature `compression`. Compresses response bodies with the best encoding the client accepts: brotli, zstd, gzip or deflate, negotiated from `Accept-Encoding` q-values (ties prefer br > zstd > gzip > deflate).

```rust
use zapline::middleware::compression::is_compressible;
use zapline::middleware::{Compression, CompressionLevel};
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .wrap(Compression::new().zstd(false).min_size(1024))
        .get("/", || async { "hello ".repeat(1000) })
}

fn tuned() -> Compression {
    Compression::new()
        .quality(CompressionLevel::Fastest)
        .content_types(|ct| is_compressible(ct) || ct == "text/event-stream")
}
```

| Method | Effect |
|---|---|
| `.gzip(bool)`, `.br(bool)`, `.zstd(bool)`, `.deflate(bool)` | Enable or disable an encoding (all on by default) |
| `.quality(CompressionLevel)` | `Fastest`, `Default` (brotli 4, zstd 3, gzip/deflate 6), `Best` or `Precise(n)` |
| `.min_size(bytes)` | Don't compress bodies known to be smaller (default 860) |
| `.content_types(\|ct\| bool)` | Which media types to compress (a lowercase type without parameters). Default: `is_compressible`. |

What it does and doesn't touch:

- It skips `HEAD`, 1xx, 204, 206 and 304 responses, responses that already have a `Content-Encoding` or a `Content-Range`, responses with `Cache-Control: no-transform`, bodies below `min_size` and content types that don't compress. By default it compresses text, JSON, JavaScript, XML, SVG, WebAssembly, YAML, TOML, forms and uncompressed fonts, and skips images, audio, video, archives and `woff`/`woff2`.
- `text/event-stream` is skipped by default, because buffering encoders delay events. Opt in with `content_types` if you need it (the encoder flushes whenever the source pauses).
- It sets `Content-Encoding`, adds `Vary: accept-encoding`, drops `Content-Length` and `Accept-Ranges`, and weakens a strong `ETag` to `W/"..."`.
- Buffered bodies up to 1 MiB are compressed in one go and keep their length. Larger and streaming bodies are compressed as they stream.
- Static files can be served pre-compressed instead; see [Static files & uploads](files.md).

## `ConcurrencyLimit`

Caps the number of requests processed at once. Beyond the cap, requests are shed with `503 Service Unavailable`, `Retry-After` and the code `overloaded`. That keeps latency bounded for the requests you accept, instead of letting everything slow to a crawl:

```rust
use std::time::Duration;
use zapline::middleware::ConcurrencyLimit;
use zapline::prelude::*;

fn app() -> App {
    // At most 512 requests in flight; wait up to 50 ms for a slot, then answer 503.
    App::new().wrap(ConcurrencyLimit::new(512).queue_timeout(Duration::from_millis(50)))
}

fn exports() -> Routes {
    // An expensive group gets its own budget.
    Routes::new()
        .get("/export", || async { "big report" })
        .wrap(ConcurrencyLimit::new(4).retry_after(5))
}
```

By default excess requests are shed immediately. `.queue_timeout(duration)` lets them wait briefly for a slot, and `.retry_after(secs)` sets the `Retry-After` value (default 1). `max()` and `available()` report the configured maximum and the free slots. A slot is held until the response head is produced, so streaming bodies, SSE and WebSockets are not counted for their whole lifetime. Clones share the limit, as do all routes under one `Routes::wrap`. Panics if `max` is 0.

## `Timeout`

Fails requests that take longer than a duration with `503 {"error":"request timed out","code":"timeout"}`:

```rust
use std::time::Duration;
use zapline::middleware::Timeout;
use zapline::prelude::*;

fn app() -> App {
    App::new().wrap(Timeout::new(Duration::from_secs(30)))
}
```

It bounds the time until the response is produced, and it drops the handler's future when the deadline passes (so cancel-safety matters for handlers that write to a database). Bodies that stream after the head, SSE and WebSockets are not cut off. To give one route a different budget, wrap that route's group: `Routes::new().get("/export", export).wrap(Timeout::new(Duration::from_secs(300)))`. Both limits apply when they are nested, and the shorter one wins. Slow *clients* (slowloris) are a separate protection, `ServerConfig::header_read_timeout` (see [Server](server.md)).

## `CatchPanic`

Turns a panic in a handler or in any middleware after it into `500 {"error":"internal server error","code":"panic"}` instead of a dropped connection. The panic message is logged, never sent to the client.

```rust
use zapline::middleware::CatchPanic;
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .wrap(CatchPanic::new().handler(|message: String| {
            eprintln!("handler panicked: {message}");
            (StatusCode::INTERNAL_SERVER_ERROR, "something broke").into_response()
        }))
        .get("/boom", || async { if true { panic!("oops") } "unreachable" })
}
```

`handler(..)` builds the response yourself from the panic message (a panic inside your handler is caught, too). Install it first (outermost) so it covers every layer after it.

**It cannot work with `panic = "abort"`.** A binary built with `[profile.release] panic = "abort"` (which the benchmark profile and the [deployment](deployment.md) example use) terminates the whole process on the first panic, so there is nothing to catch. Pick one deliberately:

- `panic = "unwind"` (the default) plus `CatchPanic`: a bug in one handler costs that one request. The process, and every other in-flight request, survives.
- `panic = "abort"`: a slightly smaller and faster binary, but any panic takes down every in-flight request on that instance and relies on your orchestrator to restart it.

For a public API with many concurrent users, the first is usually the better trade.

## `NormalizePath`

Makes `/users/`, `/users` and `//users` hit the same route. Route patterns never end in `/`, and trailing slashes are significant, so without it `GET /users/` is a 404. It must be installed with `App::wrap`: it rewrites the URI *before* routing.

```rust
use zapline::middleware::NormalizePath;
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .wrap(NormalizePath::trim_trailing_slash().merge_slashes(true))
        .get("/users", || async { "users" })   // GET /users/ and GET //users// both reach it
}

fn canonical() -> NormalizePath {
    // Send clients to the canonical URL instead of rewriting silently.
    NormalizePath::trim_trailing_slash().redirect(StatusCode::PERMANENT_REDIRECT)
}
```

| Method | Effect |
|---|---|
| `NormalizePath::trim_trailing_slash()` | `/users/` becomes `/users` (the default). The root `/` is untouched. |
| `NormalizePath::append_trailing_slash()` | `/users` becomes `/users/`. Paths whose last segment contains a `.` (like `/app.js`) are left alone. Mostly useful with `redirect` or with catch-all handlers such as static files. |
| `.merge_slashes(true)` | `/a//b` becomes `/a/b` |
| `.redirect(status)` | Answer with a redirect to the normalized URL, query string preserved. Panics if `status` is not 3xx. |

Redirect targets are always rewritten to a single leading slash, so a request for `//evil.example` can never become an open redirect.

## `SetRequestId` and `RequestId`

`SetRequestId` tags every request with a unique ID, so a log line, a trace and a client's bug report can be tied together. It reuses a valid incoming `x-request-id` (so IDs survive a hop through a proxy or another service) or generates a time-ordered UUIDv7. The ID is stored in the request, written to the request headers for downstream code, and echoed in the response header (unless the handler set it).

```rust
use zapline::middleware::{RequestId, SetRequestId};
use zapline::prelude::*;

fn app() -> App {
    App::new()
        .wrap(SetRequestId::new())
        .get("/", |id: RequestId| async move { format!("request {id}") })
}

fn strict() -> SetRequestId {
    SetRequestId::new()
        .header("x-correlation-id")
        .trust_incoming(false)                      // always generate our own
}
```

| Method | Effect |
|---|---|
| `.header(name)` | Read and write another header (default `x-request-id`) |
| `.trust_incoming(bool)` | Reuse a valid incoming ID (default `true`). Turn it off when clients are untrusted and IDs must be server-generated. |
| `.generator(\|\| String)` | Generate IDs yourself. Values that fail validation are replaced by a default ID. |

Valid IDs have at most 128 characters from `[A-Za-z0-9._:-]`, which keeps log injection out. `RequestId` is also an extractor (`id.as_str()`, `Display`); without the middleware it falls back to a valid `x-request-id` header or a fresh ID that stays stable for the rest of the request. In middleware, `req.request_id()` returns it. Install it globally so 404s get an ID too. `Trace` and `Logger` print it.

## `Cors`

Cross-Origin Resource Sharing. Install it globally, so preflight (`OPTIONS`) requests are answered before routing:

```rust
use std::time::Duration;
use zapline::middleware::Cors;
use zapline::prelude::*;

fn app() -> App {
    let cors = Cors::origins(["https://app.example.com"])
        .allow_origin_fn(|origin: &HeaderValue| origin.as_bytes().ends_with(b".preview.example.com"))
        .allow_credentials()
        .expose_headers("x-request-id, ratelimit-remaining")
        .max_age(Duration::from_secs(600));
    App::new().wrap(cors)
}
```

| Constructor / method | Effect |
|---|---|
| `Cors::permissive()` | Any origin, common methods, any request header. For development and public read-only APIs. |
| `Cors::origins([..])` | Only the listed origins. Panics on an invalid value. |
| `.allow_origin_fn(\|origin\| bool)` | Also allow origins for which the function returns `true` (for example preview deployments) |
| `.allow_methods("GET, POST")`, `.allow_headers("content-type, authorization")` | The preflight answers (`*` allows any) |
| `.allow_credentials()` | Allow cookies and `Authorization` headers; the matched origin is echoed instead of `*`, as the spec requires. Panics on `Cors::permissive()` (any origin): set `Cors::origins([..])` or `.allow_origin_fn(..)` first. |
| `.expose_headers("..")` | Response headers that scripts may read (`x-request-id`, `ratelimit-remaining`, ...) |
| `.max_age(duration)` | How long browsers cache a preflight (default 24 hours) |

Things to know:

- **Credentials require an origin allow-list.** `Cors::permissive().allow_credentials()` panics at startup, because echoing any origin with credentials would let *any* website call your API with your users' cookies. Use an explicit `Cors::origins([..])` list.
- CORS is enforced by browsers. The server still runs the request; only the response headers decide whether a script may read the result. It is not authentication.
- Responses that depend on the `Origin` get `Vary: origin`.
- Put `Cors` **outside** rate limiting, concurrency limits and authentication, so a `429`, `503` or `401` still carries CORS headers. Otherwise the browser reports an opaque CORS error instead of your real error.
- Native apps and `curl` send no `Origin` and are unaffected.

## A production stack

Global middleware runs outermost-first in the order added. This order works well, and each position is deliberate:

```rust
use std::time::Duration;
use zapline::middleware::{
    CatchPanic, Compression, ConcurrencyLimit, Cors, NormalizePath, RateLimit, SecurityHeaders,
    SetRequestId, Timeout,
};
use zapline::prelude::*;
use zapline::telemetry::Trace;

const MINUTE: Duration = Duration::from_secs(60);

async fn items() -> &'static str {
    "items"
}

async fn login() -> &'static str {
    "welcome"
}

fn app() -> App {
    let api = Routes::new()
        .get("/items", items)
        .post("/login", login)
        .rate_limit(5, MINUTE);                              // stricter, with its own bucket

    App::new()
        // 1. Identity and visibility: first, so everything below is attributed.
        .wrap(SetRequestId::new())
        .plugin(Trace::new().skip_paths(["/health/live", "/metrics"]))
        // 2. Never let a panic escape; inside Trace so it is logged as a 500.
        .wrap(CatchPanic::new())
        // 3. Canonical paths before anything keyed on the path.
        .wrap(NormalizePath::trim_trailing_slash())
        // 4. Headers on every response, including the rejections below.
        .wrap(SecurityHeaders::default())
        .wrap(Cors::origins(["https://app.example.com"]).expose_headers("x-request-id"))
        // 5. Protect the app: shed load, then bound handler time.
        .wrap(ConcurrencyLimit::new(1024).queue_timeout(Duration::from_millis(100)))
        .wrap(Timeout::new(Duration::from_secs(30)))
        // 6. Compress what the handlers produce, closest to the handlers.
        .wrap(Compression::new())
        .nest("/api", api)
        .get("/healthz", || async { "ok" })
        // Route middleware runs after routing, so per-route limits apply.
        .wrap_route(
            RateLimit::per_ip(300, MINUTE)
                .trust_forwarded_for(["10.0.0.0/8"])
                .exempt(|req: &Request| req.path() == "/healthz"),
        )
}
```

Why this order:

1. **`SetRequestId`, then `Trace`, outermost.** Every later layer, including the ones that reject a request, shows up in the logs with an ID. `Trace` records the ID, so `SetRequestId` goes first.
2. **`CatchPanic` inside `Trace`.** `Trace` then sees the `500` the panic became and logs it at `ERROR`. Panics in the outer layers are not expected.
3. **`NormalizePath` before everything keyed on paths** (CSRF exemptions, `Trace::skip_paths`, `RateLimit::exempt`), and necessarily before routing.
4. **`SecurityHeaders` and `Cors` outside the limiters.** Rejections (429, 503, 401) are responses too. They need the same headers, or the browser can't show the real error.
5. **`ConcurrencyLimit` outside `Timeout`.** A timed-out request frees its slot. Shed load as early as possible.
6. **`Compression` innermost of the global layers.** It compresses what handlers produce, and the small error bodies from the layers above skip the work. The CPU it spends is covered by the limits and the timeout.
7. **`RateLimit` as `wrap_route`.** Only route middleware can see per-route policies like `.rate_limit(5, MINUTE)`. The cost is that unmatched requests (404s) aren't rate-limited; they allocate nothing and are cheap, and `ConcurrencyLimit` still covers floods. Use `App::wrap(RateLimit::..)` instead if you need to limit 404s, and accept that policies are ignored there.

Authentication and authorization go closer to the handlers still, on route groups. See [Authentication & authorization](auth.md) for why role checks must run after routing. Add [`Csrf`](sessions.md#csrf) globally if your app serves cookie-authenticated forms. And pair this stack with `Metrics` and `Health` from [Observability](observability.md), remembering that `wrap_route` middleware also covers their routes (exempt `/healthz` and friends, as above).
