# Performance

zapline is designed so that the fastest configuration is also the default one.

## Benchmarks

`./bench/run.sh` runs identical endpoints written in zapline, axum and actix-web. The script pins servers and `wrk` to separate CPU sets, warms up, and reports the best of N runs. See `bench/README.md` for the methodology. Results from the reference machine (2 server cores) are in the project README.

Always benchmark on your own hardware, with the client and server on separate machines if possible. Numbers from a shared VM are only useful for *relative* comparison.

### Database tests (TechEmpower)

`bench/tfb/` is a complete [TechEmpower Framework Benchmarks](https://www.techempower.com/benchmarks/) implementation: plaintext, JSON, single query, multiple queries, fortunes, updates and cached queries against Postgres with TFB's schema, plus the `benchmark_config.json`, Dockerfile and a script that prepares the submission. `./bench/tfb/run.sh` starts a private Postgres cluster with TFB's settings, checks every endpoint with `verify.py` (a standalone copy of TFB's checks: headers, content types, fortunes ordering and escaping, query counts clamped to 1..500, and the number of queries per request counted with `pg_stat_statements`), then loads each test with wrk the way TFB does. See `bench/tfb/README.md`.

One local run, on a 4-vCPU VM (2 app workers, Postgres on 1 CPU, wrk on 1 CPU, 512 connections, 15 s per test), in requests per second:

| test | per-worker pool | per-worker pool + ping | `App::database` pool |
|---|---:|---:|---:|
| plaintext (16 pipelined) | 859,217 | | |
| json | 87,065 | | |
| db | 17,276 | 13,041 | 12,602 |
| queries (20) | 1,043 | 977 | 960 |
| fortunes | 17,032 | 14,089 | 14,971 |
| updates (20) | 808 | 752 | 845 |
| cached queries (100) | 18,445 | | |

Caveats: the app, Postgres and wrk share one machine, other jobs were running on it (the load average rose from about 6 to 30 during the run), and a single wrk thread limits the plaintext and JSON rows. A second run the same day gave 19,243 vs 12,541 on `db` and 18,907 vs 13,360 on fortunes for the first and last columns. These are not comparable with published TFB rounds; they are there to compare the columns with each other.

The columns are the pool modes `bench/tfb` can run in:

- **Per-worker pool**: each worker thread lazily opens its own pool (16 connections) and checks connections out without a ping. With thread-per-core runtimes a sqlx connection's socket is driven by the runtime that opened it; a per-worker pool keeps a request and its queries on one core.
- **Per-worker pool + ping**: the same, with sqlx's `test_before_acquire` (a round trip to Postgres on every checkout) back on.
- **`App::database` pool**: the canonical `App::database` + `Db` path: one pool of the same total size, opened on the startup runtime, with sqlx's defaults.

On this machine nearly all of the difference comes from the ping, not from thread affinity: the second and third columns are within noise of each other, and with Postgres on a single CPU the 20-query tests are bound by the database whatever the pool does. On machines with more cores per database the affinity effect may matter more; measure before relying on it.

### Compile times

`./bench/compile/run.sh` scaffolds apps with `zapline new` (the `minimal` and `api` templates) and builds them next to the same apps written with axum 0.8 and actix-web 4 (for `api`: the same CRUD routes with validation, an OpenAPI document with a docs UI via `utoipa` + `utoipa-scalar`, and security headers). See `bench/compile/README.md` for exactly what is measured. One run on the same 4-vCPU VM (dev profile, default linker; cold columns best of 3, edit columns median of 3):

| app | crates | cold check | cold build | rebuild after editing a handler | check after editing a handler | debug binary |
|---|---:|---:|---:|---:|---:|---:|
| zapline, minimal | 108 | 19.0 s | 27.2 s | 0.6 s | 0.3 s | 60 MB |
| axum, minimal | 48 | 10.9 s | 14.6 s | 1.4 s | 0.2 s | 22 MB |
| actix-web, minimal | 146 | 25.7 s | 34.5 s | 1.8 s | 0.3 s | 78 MB |
| zapline, api | 108 | 16.7 s | 26.1 s | 1.3 s | 0.5 s | 68 MB |
| axum, api | 95 | 16.7 s | 23.5 s | 2.5 s | 0.4 s | 50 MB |
| actix-web, api | 159 | 31.2 s | 40.2 s | 2.9 s | 0.5 s | 95 MB |

What this run shows: a zapline app costs the same to build cold whatever its size, because its default features (OpenAPI, validation, config, tracing) compile in from the start; that makes the minimal app about twice as slow to build cold as bare axum, while the API app is within 3 s of the axum equivalent and well ahead of actix-web. Rebuilding after editing a handler was the fastest of the three at both sizes; checking after an edit was within 0.1 s of the others. The VM was shared with other jobs (load average 2 to 4), so differences of a second or two in the cold columns are within noise; see `bench/compile/results.md` for the machine details.

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
- For database-heavy endpoints, measure the pool. `App::database` keeps sqlx's defaults, including a ping on every connection checkout; in the TechEmpower run above, a pool without it served about 35% more single-query requests. `bench/tfb/src/main.rs` shows a per-worker pool built from sqlx's `PoolOptions` behind a small extractor.
