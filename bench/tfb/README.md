# TechEmpower-style benchmarks

`run.sh` runs zapline, axum and actix-web through the
[TechEmpower Framework Benchmarks](https://www.techempower.com/benchmarks/)
test types on one machine, in Docker, and writes [`results.md`](results.md):
throughput, latency percentiles and memory.

```sh
./bench/tfb/run.sh                                   # everything, ~25 minutes
FRAMEWORKS="zapline actix" TESTS="db fortunes" ./bench/tfb/run.sh
```

It needs only Docker and bash: Linux, macOS, or Windows with Docker Desktop
and Git Bash. Everything else (the Rust toolchain, Postgres 17, wrk) runs in
containers. Raw wrk output for every round goes to `target/tfb/`.

## Tests

| test | request | what it does |
|---|---|---|
| `plaintext` | `GET /plaintext` | `Hello, World!` |
| `json` | `GET /json` | `{"message":"Hello, World!"}`, serialized per request |
| `path-param` | `GET /user/42` | a path parameter parsed as `u64`, returned as JSON |
| `plaintext-pipelined` | `GET /plaintext`, 16 per write | TechEmpower's plaintext test |
| `db` | `GET /db` | one random `World` row |
| `queries` | `GET /queries?queries=20` | 20 random rows, one query each |
| `fortunes` | `GET /fortunes` | all fortunes plus one added at request time, sorted, rendered as escaped HTML |
| `updates` | `GET /updates?queries=20` | read 20 random rows one by one, then write new random numbers in one batched `UPDATE` |

Before load is applied, every endpoint's response is checked (body shape,
row count, HTML escaping, the added fortune); a framework that fails a
check is reported as `failed` for that test.

## Fairness

- **Same database code.** All three frameworks call the same functions in
  [`bench/servers/src/lib.rs`](../servers/src/lib.rs), through the same
  driver (sqlx 0.8, Postgres) with the same total pool size (`POOL`, 64,
  the best of 32, 64 and 128 for both zapline and axum),
  and the same HTML renderer for fortunes. Only the HTTP layer differs.
- **Idiomatic setup.** Each framework is wired the way its documentation
  shows: axum with `axum::serve` on a multi-threaded Tokio runtime and the
  pool in `State`, actix-web with `HttpServer` and the pool in `web::Data`,
  zapline with `App::database` and the `Db` extractor. No framework gets
  hand-tuned code. One consequence worth knowing: zapline gives each
  thread-per-core worker its own pool, while actix-web's documented pattern
  shares one pool across its per-thread runtimes. (actix-web's TechEmpower
  entry is hand-tuned differently: per-worker connections and another
  driver.)
- **Same build.** Release profile with fat LTO, one codegen unit,
  `panic = "abort"` and mimalloc for all three, built by the same `rustc`.
- **Same resources.** Four workers each, on the same four CPUs.
- **Isolation.** Server, Postgres and wrk run with host networking on
  disjoint CPU sets (`SERVER_CPUS`, `DB_CPUS`, `CLIENT_CPUS`). During the
  plaintext and JSON tests the server uses all four of its CPUs while wrk
  stays below its eight, so those tests measure the server, not the client.
- **Repetition.** Each test gets a warm-up, then `ROUNDS` runs; the best
  round is reported, as TechEmpower does.

Postgres is configured like TechEmpower's database (see
[`postgres/postgresql.conf`](postgres/postgresql.conf)), including
`synchronous_commit = off`, which affects every framework equally.

## Measurements

- **Throughput:** wrk's requests/second. The percentage is relative to the
  fastest framework in that row.
- **Latency:** wrk's p50 and p99 for the same run. wrk measures latency at
  saturation and is subject to coordinated omission; compare the numbers
  with each other, not with latency SLOs.
- **Memory:** the server process's resident set size (`VmRSS`) right after
  startup, and its peak (`VmHWM`) over all tests.
- **Errors:** non-2xx responses and socket errors (wrk counts a request
  still unanswered after 2s as a timeout) are shown next to the number.

## Caveats

- Client, server and database share one machine, and on Windows and macOS
  Docker runs a Linux VM whose virtual CPUs are not pinned to physical
  cores. Treat the numbers as a comparison between frameworks on the same
  machine, not as absolute capacity. Laptops also throttle: interleave or
  repeat runs before reading much into differences under ~5%.
- Most of the CPU time in the plaintext and JSON tests is the kernel's
  loopback TCP stack (about three quarters in a `perf` profile of zapline),
  so framework differences look smaller here than over a real network
  with more client capacity.
- The `queries` and `updates` tests are largely bound by Postgres, and vary
  most between runs; repeat them interleaved before trusting a small gap.
- All three frameworks occasionally show a few hundred wrk timeouts in a
  run of millions of requests (under 0.01%). They come and go between
  rounds and frameworks, which points at VM scheduling stalls, not at
  any one server.
- Environment overrides: `FRAMEWORKS`, `TESTS`, `DURATION`, `WARMUP`,
  `ROUNDS`, `CONNS`, `PIPE_CONNS`, `PIPE_DEPTH`, `QUERIES`, `WORKERS`,
  `POOL`, `SERVER_CPUS`, `CLIENT_CPUS`, `DB_CPUS`, `WRK_THREADS`,
  `RUST_IMAGE`, `OUT`, `RAW`.
