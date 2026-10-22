# Benchmarks

For the full TechEmpower-style suite (database tests, latency percentiles,
memory, all in Docker on any OS), see [`tfb/`](tfb/README.md) and its
[latest results](tfb/results.md). This page covers the quick HTTP-only
harness.

`servers/` has three binaries that implement identical endpoints:

| endpoint | response |
|---|---|
| `GET /plaintext` | `Hello, World!` (text/plain) |
| `GET /json` | `{"message":"Hello, World!"}` |
| `GET /user/{id}` | `{"id":42,"name":"ada"}` (path param parsed as `u64`) |

All three use mimalloc and the workspace release profile (fat LTO, 1 codegen
unit, `panic = "abort"`). Each uses its framework's default server settings,
apart from the worker count and actix's backlog (set to match zapline's 4096).
`run.sh` starts them with `NO_DB=1`, which leaves out the database endpoints.

## Running

`run.sh` uses Linux's `taskset` to pin the server and client to separate CPUs, so it is Linux-only. On macOS or Windows, measure a single app with `zapline bench <url>`, which uses wrk or the cross-platform [oha](https://github.com/hatoo/oha) (`cargo install oha`). Publish comparison numbers from Linux, where zapline's thread-per-core runtime applies.

```sh
sudo apt-get install wrk bc     # or brew install wrk
./bench/run.sh
```

Environment overrides: `DURATION`, `CONNS`, `ROUNDS`, `WORKERS`,
`SERVER_CPUS`, `CLIENT_CPUS`, `WRK_THREADS`, `OUT`.

Started with `MIDDLEWARE=1`, the zapline server also installs `SetRequestId`,
`SecurityHeaders`, `Metrics` and `Trace` (with no subscriber, so tracing is
disabled), to measure what commonly used middleware costs per request.

The script pins the server and `wrk` to disjoint CPU sets, warms each
endpoint up, and keeps the best of `ROUNDS` runs. `plaintext-pipelined-16`
sends 16 requests per write, like TechEmpower's plaintext test.

## Caveats

- If the client and server share a machine, results are only good for
  relative comparison. For publishable numbers, use two machines over a real
  NIC, or submit to TechEmpower.
- axum's `axum::serve` doesn't enable hyper's `pipeline_flush`. That's why it
  falls off a cliff in the pipelined test. This is axum's out-of-the-box
  behavior, not something the harness gets wrong.
