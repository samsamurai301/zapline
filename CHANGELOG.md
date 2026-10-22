# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

The first public release.

### AI-assisted development
- `AGENTS.md` in the repository and in every project `zapline new` creates: commands, layout and the one canonical way to do each task.
- A runnable cheatsheet (`examples/examples/cheatsheet.rs`, ~1.6k tokens) that covers the common 90% of the API.
- `llms.txt` and `llms-full.txt` generated from the guide (`scripts/gen-llms.sh`) and published with the docs site.

### Core
- Radix-tree router with typed path parameters, nested route groups, per-route typed metadata, 404/405 handling and automatic HEAD.
- Extractors: `Path`, `Query`, `Json`, `Form`, `Bytes`, `String`, `State`, `Extension`, `HeaderMap`, `Method`, `Uri`, `ConnInfo`, `MatchedPath` and `Request`, plus `Option<T>` and `Result<T, Error>` wrappers. Rejections carry stable error codes and structured details.
- A single `Error` type that `?` works with: structured `{"error", "code", "details"}` bodies, extra headers, and a global `App::error_handler`.
- Middleware at three levels: global (before routing), route (after routing) and group.
- Lifecycle: async `on_startup` / `on_shutdown` hooks with a typed `Resources` registry, `Plugin`s, `on_build` hooks, and innermost `EndpointLayers`.
- `App::test()`: an in-process test client with a cookie jar, multipart and query builders, and assertion helpers.

### Server
- Thread-per-core runtimes with `SO_REUSEPORT`, or a work-stealing runtime.
- Graceful shutdown on SIGINT/SIGTERM with a drain timeout. Custom shutdown triggers.
- HTTP/2 (h2c and ALPN), TLS via rustls, Unix domain sockets, connection limits and header limits.

### API documentation and validation
- OpenAPI 3.1 generated from handler signatures, doc comments and macro arguments. Security requirements come from auth extractors and policies.
- Swagger UI, Scalar and Redoc, with pinned versions and SRI hashes.
- `Valid<T>` for `Json`, `Form`, `Query` and `Path`, returning 422 with per-field details.

### Application structure
- Dependency injection: `Dep<T>`, trait objects, async factories, `Inject`, `Cached<T>`, overrides for tests.
- Layered configuration: TOML files, profiles, `.env` and `APP__*` environment variables, plus `Secret<T>`.
- `BackgroundTasks`, scheduled jobs (`Schedule::every`, `daily_at`), and long-running workers with shutdown tokens.

### Security
- Bearer, Basic and API-key extractors, and JWT (HS/RS/ES) with `Authenticate` and `CurrentUser`.
- Route policies (`require_auth`, `require_roles`, `require_scopes`) that **fail closed**. A route with a policy but no auth middleware answers 401, or 500 `auth_misconfigured`, instead of being public.
- Cookies (plain, signed, private), sessions with a pluggable store, and CSRF protection.
- Middleware: CORS, rate limiting (per IP/key/route, trusted-proxy aware), security headers, compression, request IDs, timeouts, catch-panic, load shedding and path normalization.
- Secure defaults:
  - 500 responses never expose internal messages.
  - JSON and form extractors match the media type exactly (no CORS-safelisted bypass).
  - CORS refuses credentials with any origin.
  - Health checks hide error text unless opted in.
  - Every 401 carries a `WWW-Authenticate` challenge.

### Realtime and content
- WebSockets, Server-Sent Events, streaming request and response bodies, and NDJSON.
- Static files with ETag, Range, precompressed variants and traversal protection. Multipart uploads.

### Observability
- `tracing` integration with per-request spans.
- A Prometheus `/metrics` plugin with route-template labels and custom metrics.
- Health and readiness probes.

### Tooling
- The `zapline` CLI: `new` (minimal/api/full templates), `generate`, `dev` (live reload), `routes`, `openapi`, `doctor` and `bench`.
- A wrk benchmark harness comparing zapline with axum and actix-web, and an mdBook user guide.
