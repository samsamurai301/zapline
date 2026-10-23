# TechEmpower Framework Benchmarks

zapline's implementation of the [TechEmpower Framework Benchmarks](https://www.techempower.com/benchmarks/)
(TFB): all seven test types against Postgres with TFB's schema, a verifier
that applies TFB's checks, a local runner, and the files a TFB submission
needs.

| test | url | what it does |
|---|---|---|
| plaintext | `/plaintext` | `Hello, World!` as `text/plain` (benchmarked with 16 pipelined requests) |
| json | `/json` | `{"message":"Hello, World!"}` |
| db | `/db` | one random `World` row |
| query | `/queries?queries=N` | N random rows, one query each, N clamped to 1..500 |
| fortune | `/fortunes` | every `Fortune` plus one added at request time, sorted, rendered by a MiniJinja template |
| update | `/updates?queries=N` | N random rows read one by one, given new random numbers, written back in one `UPDATE ... FROM unnest(..)` |
| cached query | `/cached-queries?count=N` | N rows from `zapline::cache::Cache`, filled from the database at startup |

Every response carries `Server` and `Date` headers, as TFB requires.

## Files

| file | |
|---|---|
| `src/main.rs` | the app |
| `templates/fortunes.html` | the fortunes template |
| `create-postgres.sql` | TFB's schema and data (`World`, `Fortune`, both spellings) |
| `postgresql.conf` | TFB's Postgres settings, for the local cluster |
| `verify.py` | TFB's checks, standalone (Python 3, standard library) |
| `run.sh` | local cluster, build, verify, wrk, `results.md` |
| `benchmark_config.json`, `config.toml`, `zapline.dockerfile` | the TFB submission metadata and image |
| `prepare-submission.sh` | copies all of that into a FrameworkBenchmarks checkout |
| `results.md` | the last local run |

The crate is not part of the zapline workspace: TFB builds this directory on
its own, so it has its own `Cargo.lock` and release profile (fat LTO, one
codegen unit, `panic = "abort"`, mimalloc).

## Database pools: one per worker

zapline runs one single-threaded Tokio runtime per core. A sqlx connection's
socket is driven by the runtime that opened it, and `App::database` opens its
pool on the startup runtime, so with a shared pool most queries cross
threads: the worker that wrote the query waits for a wake-up from the
thread that owns the socket. This app gives each worker thread its own lazily
created pool (`DB_POOL_SIZE` connections, default 16), so a request and its
database I/O never leave their core. It also turns off sqlx's
`test_before_acquire`, which costs a round trip to Postgres on every
checkout.

`DB_POOL=shared` runs the canonical `App::database` + `Db` path instead
(sqlx's defaults, ping included), and `DB_PING=1` turns the ping back on for
per-worker pools. `run.sh` benchmarks all three (`POOLS="per-worker
per-worker-ping shared"`). In the run in `results.md` (a shared 4-vCPU VM,
Postgres on one CPU) dropping the ping was worth about a third more
single-query throughput (17,276 vs 12,602 requests/s), while per-worker
pools with the ping were within noise of the shared pool: there, the gain
came from the ping, not from thread affinity. Measure on your own hardware
before drawing conclusions about the latter.

## Running locally

```sh
sudo apt-get install postgresql postgresql-contrib wrk python3   # Debian/Ubuntu
./bench/tfb/run.sh                     # verify, then benchmark all three pool modes
VERIFY_ONLY=1 ./bench/tfb/run.sh       # only the TFB checks
DURATION=30s CONNS=256 POOLS=per-worker ./bench/tfb/run.sh
```

`run.sh` creates a private Postgres cluster (default `/var/tmp/zapline-tfb-pg`,
port 5433) with TFB's `postgresql.conf` (including `pg_stat_statements`,
which the query-count checks need), loads `create-postgres.sql` into
`hello_world` as `benchmarkdbuser`, and stops the cluster when it exits
(`KEEP_PG=1` keeps it). It runs as root too: Postgres is started as the
`postgres` user.

On a 4-CPU machine it pins the app to CPUs 0-1 (two workers), Postgres to
CPU 2 and wrk to CPU 3; on larger machines it gives half the CPUs to the app
and a quarter each to Postgres and wrk. Override with `SERVER_CPUS`,
`PG_CPUS`, `CLIENT_CPUS`, `WORKERS` and `WRK_THREADS`.

The load follows TFB's: wrk with keep-alive, TFB's `Accept` headers, an 8 s
timeout, a warm-up, then one run per test (15 s by default) at 512
connections. TFB itself sweeps concurrency (16 to 512, plaintext up to
16,384) and query counts (1 to 20, cached 1 to 100); the local run takes the
top level of each.

wrk 4.1.0 from the distribution packages (`apt-get install wrk`) was used for
`results.md`.

## Verifying

```sh
python3 bench/tfb/verify.py http://127.0.0.1:8080
python3 bench/tfb/verify.py http://127.0.0.1:8080 --psql "psql -h /var/tmp/zapline-tfb-pg -p 5433 -U postgres -d hello_world"
```

`verify.py` mirrors `toolset/test_types/` and `verifications.py` in the TFB
repository: required headers and their format (including a `Date` that is
not cached), content types, bodies, `/db` returning an object, query counts
clamped for `2`, `0`, `foo`, `501` and an empty value, the fortunes page
normalized by a copy of TFB's `FortuneHTMLParser` and compared with the
expected page (ordering, the added fortune, escaping of `<script>`, quotes
and apostrophes, UTF-8), and updates that really reach the database. With
`--psql` it also counts executed queries, rows read and rows updated with
`pg_stat_statements` around a burst of 512 requests, with TFB's margins, and
compares every row an update returned with the stored row. It exits non-zero
on any failure.

## Submitting to TechEmpower

1. Make sure the zapline commit you want benchmarked is on GitHub (or
   published to crates.io).
2. Fork and clone <https://github.com/TechEmpower/FrameworkBenchmarks>, then:

   ```sh
   ./bench/tfb/prepare-submission.sh ~/FrameworkBenchmarks          # zapline from git at HEAD
   # or, once the version on crates.io has these features:
   ZAPLINE_DEP=crates ./bench/tfb/prepare-submission.sh ~/FrameworkBenchmarks
   ```

   This writes `frameworks/Rust/zapline/` with the app, `Cargo.lock`,
   `benchmark_config.json`, `config.toml`, `zapline.dockerfile` and a
   README, with the `zapline` dependency pointing at git (or crates.io)
   instead of this repository's path. The app only uses public zapline
   APIs, so `ZAPLINE_REV=<sha>` can name any pushed commit that has them
   (the default is `HEAD`, which must be pushed first).
3. Run TFB's own verification, which builds the image and starts its
   Postgres container (needs Docker):

   ```sh
   cd ~/FrameworkBenchmarks
   ./tfb --mode verify --test zapline
   ./tfb --mode benchmark --test zapline   # optional: a full local run
   ```

4. Commit `frameworks/Rust/zapline/` on a branch and open a pull request
   against `TechEmpower/FrameworkBenchmarks` (their CI runs the same verify
   step). Results appear in the next continuous run on their hardware
   (<https://tfb-status.techempower.com/>) and in the following official
   round.

To test the image without pushing anything, build a self-contained copy
with zapline vendored in:

```sh
ZAPLINE_DEP=vendor ./bench/tfb/prepare-submission.sh /tmp/zapline-tfb
docker build -f /tmp/zapline-tfb/zapline.dockerfile -t zapline-tfb /tmp/zapline-tfb
docker run --rm --network host -e DATABASE_URL=postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1:5433/hello_world zapline-tfb
```

TFB's rules to keep in mind when changing the app: each query in `/queries`
and `/updates` must be a separate round trip (batching the updates is
allowed), `/fortunes` must read the table and render through a template on
every request, the cached-queries cache must be a real cache rather than a
precomputed response, and nothing may be cached across requests for the
other tests.
