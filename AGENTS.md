# AGENTS.md

Instructions for AI coding assistants (and humans) working on the zapline
repository itself. If you are building an *app* with zapline, read
[`docs/src/cheatsheet.md`](docs/src/cheatsheet.md) instead; projects created
with `zapline new` get their own `AGENTS.md`.

## Layout

| Path | What |
|---|---|
| `crates/zapline` | The framework (core + feature modules behind Cargo features) |
| `crates/zapline-macros` | `#[get]`/`#[post]`/... route macros |
| `crates/zapline-cli` | `zapline` / `cargo zapline`: new, generate, dev, routes, openapi, doctor, bench. Templates in `templates/*.tmpl` |
| `examples/examples` | Runnable examples; `cheatsheet.rs` is the canonical-usage file included in the guide |
| `examples/ecosystem` | sqlx / SeaORM / Diesel / Leptos example apps: their own workspace and lockfile, checked by `./scripts/check.sh ecosystem` |
| `docs/src` | The mdBook guide. `llms.txt` / `llms-full.txt` are generated from it |
| `bench/` | zapline vs axum vs actix-web harness (`./bench/run.sh`) |

[`ARCHITECTURE.md`](ARCHITECTURE.md) has the request lifecycle, the extension
points, and the design rules. Read its "Design rules" before touching the hot
path.

## Commands

```sh
./scripts/check.sh quick                  # fmt + clippy + tests: run before every commit
./scripts/check.sh                        # + feature matrix, examples, docs (what CI runs)
./scripts/check.sh ecosystem              # + the examples/ecosystem workspace (heavy deps)
cargo test -p zapline --all-features --test <name>   # one integration test file
./scripts/gen-llms.sh                     # regenerate docs/src/llms*.txt after editing docs
./bench/run.sh                            # before/after numbers for hot-path changes
```

Windows: `.\scripts\check.ps1`.

## Rules

- No per-request cost for unused features: never add fields to `Request` or
  work to `AppInner::handle` for one feature. Do build-time work in `on_build`
  or plugin install instead.
- No `unsafe` outside the server, and no panics on request paths.
- Every public item gets a doc comment with a short example. New behavior gets
  an integration test in `crates/zapline/tests/`.
- New Cargo features must compile alone: add them to the feature loop in
  `scripts/check.sh` and `.github/workflows/ci.yml`.
- Changing a CLI template means updating the expected-file lists in
  `crates/zapline-cli/src/new.rs` tests.
- Keep the public surface small: one obvious way per task. If you add an
  alternative API, justify it in the PR.
- MSRV is 1.88 (`rust-version` in `Cargo.toml`): don't use newer std APIs.
