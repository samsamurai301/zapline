# Contributing to zapline

Thanks for helping! Every kind of contribution counts: a bug report with a
reproduction, a docs fix, a new example, a failing test, or a feature. If you
are new to Rust web frameworks, this is a good place to learn one from the
inside: the codebase is small for what it does, and every module has
integration tests that show how it behaves.

## Your first contribution in 15 minutes

```sh
git clone https://github.com/samsamurai301/zapline && cd zapline
./scripts/check.sh quick        # fmt + clippy + tests (Windows: .\scripts\check.ps1 quick)
cargo run -p zapline-examples --example hello   # http://localhost:8080
```

Then pick something:

- Issues labeled [`good first issue`](https://github.com/samsamurai301/zapline/labels/good%20first%20issue)
  are small, self-contained and have a pointer to the file to change.
- Found a typo or an unclear paragraph in the [guide](https://samsamurai301.github.io/zapline/)?
  The source is `docs/src/*.md`; fix it and open a pull request.
- Tried zapline and something surprised you? That is a bug in the docs or
  the API. Open an issue and say what you expected.

## How the code is organized

| Path | What |
|---|---|
| `crates/zapline` | The framework: core (`app.rs`, `router.rs`, `request.rs`, `extract.rs`, `server/`) and feature modules behind Cargo features |
| `crates/zapline-macros` | `#[get]`/`#[post]`/`#[model]` macros |
| `crates/zapline-codegen` | Typed TypeScript/JavaScript clients from OpenAPI |
| `crates/zapline-cli` | The `zapline` command: `new`, `dev`, `generate`, `ship`, `mcp`, ... |
| `examples/` | Runnable examples, plus the `saas` app and the ecosystem (sqlx, SeaORM, Diesel, Leptos) workspace |
| `docs/src` | The mdBook guide |
| `bench/` | Benchmarks against axum and actix-web, the TechEmpower app, and the chaos suite |
| `fuzz/` | Coverage-guided fuzzing of request handling |

[`ARCHITECTURE.md`](ARCHITECTURE.md) walks through a request from accept to
response and lists the extension points every feature is built on. Read its
**Design rules** before changing the hot path.

## The rules that keep zapline fast and reliable

1. **No per-request cost for unused features.** Never add fields to `Request`
   or work to `AppInner::handle` for one feature. Do work at build time.
2. **No `unsafe`** (the compiler enforces it) and **no panics on request
   paths**. Malformed input is a 4xx, never a crash or a 500.
3. **Every public item has a doc comment with a short example**, and new
   behavior gets an integration test in `crates/zapline/tests/`.
4. **New Cargo features compile alone**: add them to the feature loop in
   `scripts/check.sh` and `.github/workflows/ci.yml`.
5. **One obvious way per task.** If you add an alternative API, say why in
   the pull request.
6. **MSRV is 1.88**: don't use newer std APIs.

## Checks

```sh
./scripts/check.sh quick        # before every commit: fmt, clippy, tests
./scripts/check.sh              # what CI runs: + feature matrix, examples, docs
cargo test -p zapline --all-features --test <name>   # one test file
./scripts/gen-llms.sh           # after editing docs/src: regenerates llms.txt
cargo deny check                # supply chain: advisories, licenses, sources
./scripts/coverage.sh           # line coverage vs .github/coverage-floor (needs cargo-llvm-cov)
```

The coverage floor only goes up. CI fails when coverage drops under it, or
when a pull request lowers the number. When your tests raise coverage, raise
the floor in the same pull request.

### Reliability checks

When you touch the server, body handling, extractors or middleware, also run:

```sh
./bench/chaos/run.sh            # hostile traffic against a real server (about 4 minutes)
cargo +nightly fuzz run request -- -max_total_time=300   # needs `cargo install cargo-fuzz`
```

`bench/chaos` starts a real zapline server and attacks it: slowloris, slow
bodies, clients that never read, malformed and smuggling-style requests,
oversized inputs, 60,000 abrupt disconnects, file descriptor exhaustion,
HTTP/2 rapid reset and floods, WebSocket abuse, and a mixed-load soak that
watches memory. Every scenario must pass. `fuzz/` sends arbitrary requests
through most extractors and middleware and fails on any panic or 5xx.

### Performance changes

If you touched the hot path (`server/`, `router.rs`, `app.rs`, `handler.rs`,
`request.rs`), run `./bench/run.sh` before and after and paste both tables
into the pull request. Performance changes need numbers.

## Pull requests

- Keep a pull request to one change. Small ones get reviewed fastest.
- Fill in the template: what changed, why, and how you tested it.
- API changes need a test, a doc comment with an example, and a guide update.
- Diagnostics: a mistake a user can make when putting an app together gets a
  stable code in `crates/zapline/src/codes.in` (see `ARCHITECTURE.md`).

## Reporting bugs and security issues

Bugs: open an issue with the smallest program that shows the problem and the
output you expected. Security vulnerabilities: please report them privately,
as described in [`SECURITY.md`](SECURITY.md).

## Code of conduct

Be kind and assume good intent. Harassment or personal attacks of any kind
are not tolerated; maintainers will remove comments and block accounts that
cross that line. The full text is the
[Contributor Covenant 2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).

By contributing, you agree that your work is licensed under the project's
MIT OR Apache-2.0 license.
