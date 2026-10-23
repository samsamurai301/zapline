# Roadmap & handoff notes

This file tracks what is done, what was deliberately deferred, and what comes next. It's kept up to date for contributors.

## Status (0.1.0, pre-release)

| Area | State |
|---|---|
| Core: routing, extractors, errors, middleware, lifecycle, plugins, test client | done |
| Server: thread-per-core, graceful shutdown, HTTP/2, TLS, Unix sockets, limits | done |
| OpenAPI 3.1 and docs UIs, validation | done |
| DI, layered config, background tasks and scheduled jobs | done |
| Auth: Bearer, Basic, API key, JWT, policies (fail-closed), cookies, sessions, CSRF (signed) | done |
| Middleware suite: CORS, compression, rate limit, request IDs, security headers, timeouts, catch-panic, load shedding, path normalization | done |
| Realtime and content: WebSockets, SSE, streaming, static files, multipart | done |
| Observability: tracing, Prometheus metrics, health | done |
| CLI: new, generate, dev, routes, openapi, doctor, bench | done |
| Guide (mdBook, every snippet compile-checked) and examples | done |
| Ecosystem examples: sqlx transactions, SeaORM, Diesel, Leptos SSR (`examples/ecosystem`) | done |
| Security review (5 dimensions) and fixes | done. Findings and fixes are in the git history ("Fix review findings ...") |

Run `./scripts/check.sh` to verify everything locally.

## Deferred, with reasons

- **WebSocket connections and `max_connections`/drain.** Upgraded connections release their connection slot at upgrade, and the shutdown drain doesn't track them. Fixing this means wrapping every connection's IO type. That needs a careful, measured change: an earlier change of that kind cost about 2% of instructions per request by blocking LTO inlining. Documented in `docs/src/server.md`.
- **hyper 1.12 + `pipeline_flush`.** Malformed or oversized request heads close the connection without the 400/431 response. This is an upstream issue; `ServerConfig::pipeline_flush = false` works around it. Report it upstream and re-test on new hyper releases.
- **CSRF tokens aren't bound to a session.** They are signed when a key is set, and the `__Host-` cookie prefix prevents injection by sibling domains. Binding tokens to the session id when `SessionLayer` is present would add defense in depth.
- **The test cookie jar** ignores `Domain`, `Secure` and `SameSite`.
- **OpenAPI:** credential schemes are matched by kind only. A custom `ApiKeyConfig` header name isn't visible at build time, so declare `api_key_auth(name, header)` explicitly.

## Next milestones

1. **Release:** push to the private repository `samsamurai301/zapline` (run `publish-to-github.ps1` or `.sh`) and watch the first CI run on Linux, macOS and Windows. When going public, publish `zapline-macros`, `zapline` and `zapline-cli` to crates.io (in that order) and the guide with GitHub Pages.
2. **Benchmarks:** run `./bench/run.sh` on dedicated hardware with separate client and server machines. Prepare a TechEmpower Framework Benchmarks submission (needs the database tests: single query, multiple queries, fortunes, updates).
3. **Distributed stores:** pluggable rate-limit and session stores (Redis), and JWKS / key rotation for JWT.
4. **Scheduling:** full cron expressions, with job metrics.
5. **Telemetry:** an OpenTelemetry OTLP exporter and trace-context propagation (`traceparent`).
6. **Protocols:** WebSockets over HTTP/2 (RFC 8441), with HTTP/3 as an experiment.
