### Throughput (requests/second, higher is better)

| test | zapline | axum | actix |
|---|---:|---:|---:|
| plaintext | **402403 (100%)**, 468 errors | 283615 (70%), 501 errors | 313881 (78%) |
| json | **403137 (100%)** | 274256 (68%) | 345397 (86%) |
| path-param | **390350 (100%)** | 256877 (66%) | 323798 (83%), 367 errors |
| plaintext-pipelined | **2490719 (100%)** | 487313 (20%), 1 errors | 1949173 (78%) |
| db | **44755 (100%)** | 35181 (79%) | 26426 (59%) |
| queries | 5700 (82%) | **6991 (100%)** | 5277 (75%) |
| fortunes | **32208 (100%)** | 29879 (93%) | 19438 (60%) |
| updates | **5864 (100%)** | 4757 (81%) | 4350 (74%) |

### Latency under that load (p50 / p99 ms, lower is better)

| test | zapline | axum | actix |
|---|---:|---:|---:|
| plaintext | 1.09 / 2.90 | 1.70 / 4.93 | 1.48 / 3.12 |
| json | 1.10 / 3.01 | 1.75 / 5.04 | 1.32 / 3.09 |
| path-param | 1.12 / 2.83 | 1.87 / 5.52 | 1.41 / 3.25 |
| plaintext-pipelined | 3.56 / 12.10 | 23.98 / 47.96 | 4.62 / 16.08 |
| db | 10.22 / 20.71 | 12.15 / 27.50 | 16.43 / 33.98 |
| queries | 93.06 / 135.84 | 59.08 / 137.79 | 75.40 / 189.94 |
| fortunes | 15.57 / 27.44 | 14.55 / 28.72 | 28.34 / 40.02 |
| updates | 72.67 / 150.76 | 122.94 / 159.92 | 90.92 / 203.55 |

### Memory (resident set size, MB, lower is better)

| | zapline | axum | actix |
|---|---:|---:|---:|
| idle, after startup | 14.8 | 10.3 | 16.3 |
| peak, across all tests | 31.1 | 47.2 | 52.4 |

_4 server workers on CPUs 0-3, Postgres on CPUs 12-19, wrk 8 threads on CPUs 4-11; 512 connections (pipelined: 1024 × depth 16); queries/updates at 20 queries; pool 64; best of 3 × 15s after a 5s warm-up._
_rustc 1.99.1 (58cf8f9c3 2026-10-13); postgres (PostgreSQL) 17.11 (Debian 17.11-1.pgdg12+2); Docker Desktop (containerized), kernel 6.18.54.1-microsoft-standard-WSL2, 20 CPUs; 12th Gen Intel(R) Core(TM) i7-12700H._

### Re-check: `queries`, interleaved

The `queries` row above is the one test zapline did not win in this run.
Its run-to-run spread on this laptop was wide (zapline 5.4k to 7.9k req/s
across runs), so it was repeated as six alternating zapline/axum pairs,
15s each, changing which framework went first:

| pair | 1 | 2 | 3 | 4 | 5 | 6 | median |
|---|---:|---:|---:|---:|---:|---:|---:|
| zapline | 7,529 | 7,439 | 7,640 | 5,788 | 6,346 | 7,586 | **7,484** |
| axum | 6,580 | 6,542 | 6,191 | 5,220 | 5,778 | 6,187 | 6,189 |

zapline was ahead in all six pairs.
