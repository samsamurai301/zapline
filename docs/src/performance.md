# Performance

zapline is designed so that the fastest configuration is also the default one.

## Benchmarks

`./bench/run.sh` runs identical endpoints written in zapline, axum and actix-web. The script pins servers and `wrk` to separate CPU sets, warms up, and reports the best of N runs. See `bench/README.md` for the methodology. Results from the reference machine (2 server cores) are in the project README.

Always benchmark on your own hardware, with the client and server on separate machines if possible. Numbers from a shared VM are only useful for *relative* comparison.

## What makes it fast

1. **Thread-per-core with `SO_REUSEPORT`.** Each worker thread runs its own single-threaded Tokio runtime and its own listening socket, and the kernel load-balances new connections across them. A connection never migrates between threads, and there's no work-stealing scheduler on the hot path.
2. **Core-local reference counts.** Each worker holds its own `Arc` handles to app data. Per-request clones only touch the current core's cache lines.
3. **Zero-allocation routing.** Lookup is a radix tree followed by a fixed-slot method table. Route parameters are byte ranges into the path, inline for up to 4. The matched route is a `&'static` pointer.
4. **One allocation per routed request.** The only allocation is the boxed handler future. A 404 or 405 allocates nothing.
5. **Pipelining.** Responses to pipelined HTTP/1.1 requests are flushed together.
6. **Build-time work.** OpenAPI specs, metrics series and security header values are computed once at startup, not on each request.

## Tuning

| Knob | Default | Notes |
|---|---|---|
| `App::workers(n)` | CPU count | One runtime per core |
| `ServerConfig::runtime` | `PerCore` on Linux and FreeBSD | Use `MultiThread` when handlers share heavy I/O resources and work-stealing helps |
| `ServerConfig::header_read_timeout` | 30s | Slowloris protection. It costs a timer per request. `None` disables it. |
| `App::body_limit` | 2 MiB | |
| Allocator | system | `mimalloc` gives a noticeable boost and is used in the benchmarks |
| Release profile | — | `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` |

## Guidelines for app code

- Wrap shared state in `Arc` and avoid global locks on the request path. Prefer sharded or lock-free structures.
- Use `wrap_route` instead of `wrap` for middleware that only needs matched routes. That way it doesn't run on 404s.
- Return `Bytes` or `&'static str` for static payloads. Both are zero-copy.
- Run blocking work with `tokio::task::spawn_blocking`.
