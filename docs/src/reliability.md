# Reliability

How zapline is tested beyond its unit and integration tests, what those
tests found, and where it is still weaker than frameworks with years of
production use. Everything here can be re-run from the repository.

## How it is tested

| Check | What it does | Command |
|---|---|---|
| Test suite | About 1,900 tests: in-process and real-socket integration tests for every module, CLI and code generator | `./scripts/check.sh` |
| Chaos suite | Starts a real server and attacks it over real sockets (scenarios below) | `./bench/chaos/run.sh` |
| Soak | Mixed keep-alive load for as long as you like, sampling memory to find leaks | `./bench/chaos/run.sh soak soak=3600` |
| Fuzzing | Coverage-guided (libFuzzer) arbitrary requests through path params, query, form, JSON, multipart, cookies (plain, signed, private), sessions, CSRF, JWT, i18n, static files, compression, CORS and path normalization. Any panic or 5xx fails | `cargo +nightly fuzz run request` |
| Real deployment | `zapline ship` to a systemd server over SSH, under load | see below |
| Supply chain | RustSec advisories, licenses, crate sources | `cargo deny check`, `cargo audit` |
| No `unsafe` | `#![forbid(unsafe_code)]` in every crate: the compiler rejects any `unsafe` block | built in |

## Chaos scenarios

Each scenario starts a fresh server, attacks it, then checks that the
process is alive, still answers a normal request quickly, and gave back its
file descriptors.

| Scenario | Result |
|---|---|
| 5,000 connections of random bytes | No 5xx, server healthy |
| 24 malformed and smuggling-style requests (bad methods, bare CR, obs-fold, conflicting `Content-Length`, bad chunk sizes, 100 KB URIs, 1 MB headers, 1,000 headers, invalid UTF-8, 100,000-deep JSON, broken multipart, ...) | Each gets the right 4xx (400, 413, 414, 415, 422, 431) |
| Slowloris: 1,000 connections sending one header byte at a time | All cut at `header_read_timeout`; other clients answered in under 1 ms meanwhile |
| 500 idle keep-alive connections | All closed by the server at the timeout |
| 200 request bodies sent at 2 bytes/s | `408` and closed at `body_read_timeout` |
| 60,000 connections dropped at every stage (mid-head, mid-body, mid-response, mid-upgrade, with RST) | Memory flat between rounds, no descriptor leak |
| 3 MiB bodies (with `Content-Length`, chunked, multipart), 150,000 form fields | `413` before the body is read; huge forms handled |
| 2,000 panicking handlers | Server stays up (500s with `CatchPanic`) |
| 600 connections against a 256 file-descriptor limit | Accept loop backs off (1% CPU), recovers when descriptors free up |
| HTTP/2 rapid reset (CVE-2023-44487), PING flood, SETTINGS flood | GOAWAY after a few thousand resets, memory flat |
| WebSocket frame claiming 2^62 bytes; 2,000 open sockets; 50,000 echoes | Closed at once; 23 MiB for 2,000 sockets |
| Soak: 128 keep-alive connections, mixed routes | 10 minutes: 130 million requests (216,000/s), 0 errors, p99 2.5 ms, memory flat at 9 MiB |

## Real deployment

`zapline ship` was run against a real systemd server (Ubuntu 24.04, systemd
255, a sudo login over SSH) while 16 clients sent requests continuously:

- A new release swapped in during 217,224 requests with **0 failures**
  (responses switched from v1 to v2 mid-run).
- `--rollback` under the same load: **0 failures**.
- A release that panics at startup: the deploy stopped with the release's
  log lines, and the old release kept serving all 154,434 requests.
- Settings edited on the server and the generated secrets survived the next
  deploy.

The dev loop was checked in a real browser (Chromium): a template edit
reloads the page in about 0.4 s without a rebuild, a CSS edit swaps the
stylesheet in about 0.1 s keeping page state, and a compile error shows an
overlay with the file, line and compiler output that clears itself once the
code is fixed.

## What the audit fixed

- **Slow request bodies held connections forever.** New
  `ServerConfig::body_read_timeout` (default 30s) answers `408`.
- **Malformed requests got a dropped connection instead of a 4xx** because
  `pipeline_flush` was on by default (a hyper bug, see
  [upstream issues](upstream-issues.md)). It is now off by default; only the
  benchmarks turn it on.
- **WebSockets used 128 KiB each** (tungstenite's read buffer). They now
  start at 8 KiB: 2,000 sockets went from 258 MiB to 23 MiB.
- **`zapline ship` regenerated the server's settings on every deploy** when
  logged in with sudo: it tested for the settings file without root, could
  not see it, and wrote a new one (new secrets, default port). Fixed, with a
  test that every access to the root-only directories goes through `sudo`.

## Where zapline is weaker than mature frameworks

Honest gaps, so you can decide where it fits today:

- **Production mileage.** axum and actix-web have run in production for
  years at large scale. zapline's tests are thorough, but nothing replaces
  that. Run it behind a reverse proxy or load balancer for internet-facing
  apps until you have your own confidence.
- **No write timeout.** A client that stops reading a response keeps its
  connection, and the unsent part of the response, until it disconnects (200
  such clients held about 800 MiB of 4 MiB responses). axum has the same
  behavior; a reverse proxy, `max_connections`, or streaming large
  responses bounds it.
- **Streaming bodies set their own pace.** `body_read_timeout` covers
  buffered bodies; `BodyStream` and `Multipart` handlers should bound their
  own uploads.
- **Young code, small team.** Bugs like the `zapline ship` settings one are
  what a young project still has. The chaos suite and fuzzer exist so the
  next one is found by a test, not by a user.
- **HTTP/3 is experimental**, and HTTP parsing is hyper's (well fuzzed and
  widely deployed, which is a strength).
