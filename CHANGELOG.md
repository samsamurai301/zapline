# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

The first public release.

### Full-stack
- `zapline::db` (features `postgres`, `sqlite`): sqlx pool created at startup, SQL migrations applied automatically, `Db` extractor, `DbCheck` readiness check, `Database::sqlite_memory()` for isolated tests, and sqlx errors mapped to 404/409/422/503.
- `zapline::view` (feature `views`): MiniJinja templates with hot reload in debug builds, `render_block`/`page_or_block` for htmx fragments, `HxRequest` and `Hx` response headers.
- `zapline new --template fullstack`: htmx pages, SQLite with migrations (zero setup), JSON API, OpenAPI, Dockerfile.
- `zapline generate resource <name> field:type...`: migration, sqlx CRUD API, htmx page and tests in one command.
- `zapline db new` / `zapline db migrate`.
- `zapline g auth`: accounts in one command (users migration, sign-up/log-in/log-out as JSON API and htmx pages, `CurrentUser` extractor, tests).
- `zapline::auth::password` (feature `password`): Argon2id hashing on the blocking pool, with `verify_missing` against user enumeration by timing, and `new_token` / `hash_token` for single-use links.
- `zapline g auth` includes password reset by email: hashed single-use tokens that expire after an hour, JSON endpoints and htmx pages, and tests.
- `zapline::auth::oauth` (feature `oauth`): log in with GitHub, Google or any OAuth 2 provider. `App::oauth` mounts `/auth/{provider}`; the `OAuthLogin` extractor checks state, exchanges the code with PKCE and returns a normalized profile (GitHub's verified primary email included). `zapline g auth` wires it in: "Continue with GitHub/Google" buttons when the credentials are set, identities linked by verified email, and a test against a stand-in provider.
- `zapline::admin`: `App::admin` serves an admin panel for every table on SQLite and Postgres, read from the schema: search, sorting, paging, typed create/edit/delete forms, secret-looking columns hidden. Debug builds and localhost only unless `.protect(middleware)`; `zapline g auth` opens it to the accounts in `ADMIN_EMAILS`, and the `fullstack` template turns it on.
- `zapline::i18n` (feature `i18n`): translations from one TOML file per language, the visitor's language from `?lang=` (remembered in a cookie), the cookie or `Accept-Language`, fallbacks to the default language, CLDR plural rules for common languages, `t(...)` and `t.plural(...)` in templates, and `I18n::missing()` for tests.
- `zapline::redis` (feature `redis`): a built-in RESP2 client with connection reuse and reconnects (`Redis::cmd` for any command), and Redis-backed versions of the in-memory pieces for running several instances: `Cache::redis` (degrades to misses when Redis is down), `Live::redis` (updates reach every instance, in order, without duplicates), `RateLimit::redis` (GCRA in a Lua script on Redis's clock; fails open) and `RedisStore` for sessions.
- OpenTelemetry traces (feature `otlp`): request spans (`GET /users/{id}`), your own spans and their events exported over OTLP/HTTP JSON from a background thread, continuing W3C `traceparent`; on with `OTEL_EXPORTER_OTLP_ENDPOINT` or `Telemetry::otlp`, no OpenTelemetry SDK needed.
- `App::assets(prefix, dir)` (feature `fs`): static files with content-hashed URLs (`Assets::url`, `{{ asset('app.css') }}` in templates) served with a one-year `immutable` cache; no build step. The `fullstack` template uses it.
- `zapline::jobs`: durable background jobs in the app's database (SQLite or Postgres), with retries and backoff, `FOR UPDATE SKIP LOCKED` on Postgres so instances share one queue, and `run_pending` for deterministic tests.
- `zapline g job <name>`: a durable job, registered in `src/jobs/mod.rs` and `app()`, with a test.
- `zapline::db::Listing`: pagination, multi-key sorting, typed filters, case-insensitive search and scopes from the query string, with allow-listed columns and bound values, on SQLite and Postgres; returns `Paged<T>` (`items`, `total`, `limit`, `offset`). `zapline g resource` uses it, so generated list endpoints accept `?sort=`, `?q=` and per-field filters.
- Framework routes (`/_dev`, `/_dev/mail`, `/files`) are kept out of the OpenAPI spec and generated clients.
- `zapline::live`: pub/sub to browsers. `publish(topic, event, data)`, `sse(topic)` for htmx's SSE extension, `subscribe(topic)` for WebSockets and tests. Slow subscribers skip instead of blocking. The fullstack template pushes every todo change to all open pages.
- `zapline::cache` (feature `cache`): `Cache` with `get_or_set` that computes once under concurrent misses, `delete_prefix` invalidation and bounded memory; `ResponseCache` route middleware for anonymous `GET` pages (htmx-aware keys, skips cookies and private responses); `ETag` middleware with `304 Not Modified`.
- `zapline::storage` (features `storage`, `s3`): one API over a local directory, memory and S3-compatible buckets (AWS, R2, MinIO). Keys are validated against path escapes, `unique_key` names uploads, and `url(key, ttl)` makes expiring links: presigned on S3, HMAC-signed and served by the app otherwise. Built-in SigV4 client, no AWS SDK.
- `App::dev_console()`: a page at `/_dev` with every route, the last 50 requests (status and latency), the job queue with its latest failures and a retry button, and the dev inbox. Debug builds and localhost only; on in the `api`, `full` and `fullstack` templates.
- `examples/saas`: a complete multi-user app (accounts, password reset, per-user data, a welcome email job, a schedule) built from `zapline new` + `g auth` + `g job`.
- `zapline::mail` (feature `mail`): SMTP email, a dev inbox at `/_dev/mail` when `SMTP_URL` is unset in debug builds, `Mailer::sent` for tests, and `Email` as a durable job.
- `App::database_sessions`: sessions stored in the database (`SqlSessionStore`, SQLite and Postgres). `cookies::key_from_env` / `key_from_secret` derive the cookie key from a secret, with a development fallback in debug builds.
- `TestResponse::assert_text` and `assert_contains`.
- `zapline client`: a typed, dependency-free TypeScript client generated from the OpenAPI document; `zapline dev --client <file>` regenerates it after every build for end-to-end types.
- `zapline::extract::{empty_as_none, checkbox}` serde helpers for HTML forms.
- `ZAPLINE_DEV=1` (set by `zapline dev`) shows the real cause of 500 errors.

### AI-assisted development
- `AGENTS.md` in the repository and in every project `zapline new` creates: commands, layout and the one canonical way to do each task.
- A runnable cheatsheet (`examples/examples/cheatsheet.rs`, ~1.3k tokens; ~2k with its page) that covers the common 90% of the API.
- `llms.txt` and `llms-full.txt` generated from the guide (`scripts/gen-llms.sh`) and published with the docs site.
- `zapline mcp`: a Model Context Protocol server with `docs` (section-level answers from a guide built into the binary), `new_project`, the generators, `db_new_migration`, `db_migrate`, `check`, `test`, `routes` and `openapi`.
- `#[model]`: one attribute derives `Debug`, `Clone`, `Serialize`, `Deserialize`, `JsonSchema` and `Validate` through zapline's re-exports, so an app needs no `serde`, `schemars` or `validator` dependency. The templates, generators and examples use it.
- `zapline check [--clippy]` and `zapline test [filter]`: cargo results condensed to one line per diagnostic and only the failing tests (68–79% fewer tokens than cargo for a failing test or a type error, see `bench/tokens`).
- `bench/tokens`: a token-efficiency benchmark. The same validated, documented, tested CRUD API takes 641 tokens in zapline, 1,257 in axum and 1,336 in actix-web (o200k_base).

### Core
- OpenAPI operation ids for same-named handlers in different modules are prefixed with the module (`users_list`, `posts_list`) instead of numbered.
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
