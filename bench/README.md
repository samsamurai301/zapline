# Benchmarks

`servers/` has three binaries that implement identical endpoints:

| endpoint | response |
|---|---|
| `GET /plaintext` | `Hello, World!` (text/plain) |
| `GET /json` | `{"message":"Hello, World!"}` |
| `GET /user/{id}` | `{"id":42,"name":"ada"}` (path param parsed as `u64`) |

All three use mimalloc and the workspace release profile (fat LTO, 1 codegen
unit, `panic = "abort"`). Each uses its framework's default server settings,
apart from the worker count and actix's backlog (set to match zapline's 4096).

## Running

```sh
sudo apt-get install wrk bc     # or brew install wrk
./bench/run.sh
```

Environment overrides: `DURATION`, `CONNS`, `ROUNDS`, `WORKERS`,
`SERVER_CPUS`, `CLIENT_CPUS`, `WRK_THREADS`, `OUT`.

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
