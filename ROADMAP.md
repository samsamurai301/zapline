# Roadmap & handoff notes

This file tracks what is done, what was deliberately deferred, and what comes next. It's kept up to date for contributors.

## Status (0.1.0, pre-release)

| Area | State |
|---|---|
| Core: routing, extractors, errors, middleware, lifecycle, plugins, test client | done |
| Server: thread-per-core, graceful shutdown, HTTP/2, TLS, Unix sockets, limits (WebSockets included in `max_connections`/`max_upgraded` and the drain) | done |
| OpenAPI 3.1 and docs UIs, validation | done |
| DI, layered config, background tasks and scheduled jobs | done |
| Auth: Bearer, Basic, API key, JWT, policies (fail-closed), cookies, sessions, CSRF (signed) | done |
| Middleware suite: CORS, compression, rate limit, request IDs, security headers, timeouts, catch-panic, load shedding, path normalization | done |
| Realtime and content: WebSockets, SSE, streaming, static files, multipart | done |
| Observability: tracing, Prometheus metrics, health | done |
| CLI: new, generate, dev, routes, openapi, doctor, bench | done |
| Guide (mdBook, every snippet compile-checked) and examples | done |
| Security review (5 dimensions) and fixes | done. Findings and fixes are in the git history ("Fix review findings ...") |

Run `./scripts/check.sh` to verify everything locally.

## Deferred, with reasons

- **hyper 1.12 + `pipeline_flush`.** Malformed or oversized request heads close the connection without the 400/414/431 response, and buffered responses to earlier pipelined requests are lost with it. zapline can't answer for hyper: only hyper knows what was in its dropped write buffer, so a zapline-written 400 could answer the wrong pipelined request. The cause and a verified two-line fix are in the issue draft in `docs/src/upstream-issues.md`: file it, and re-test on new hyper releases (the canary test `hyper_drops_automatic_error_responses_with_pipeline_flush` fails once it is fixed). Until then `ServerConfig::pipeline_flush = false` works around it.
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
7. **Database guides:** sqlx, SeaORM and Diesel examples with DI and health checks, and transactional patterns.
