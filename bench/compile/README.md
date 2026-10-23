# Compile-time benchmark

How long a zapline app takes to build and rebuild, next to the same app
written with axum and with actix-web.

```sh
./bench/compile/run.sh                  # both sizes, all three frameworks, writes results.md
ROUNDS=5 TIERS=api ./bench/compile/run.sh
```

## The apps

| size | zapline | axum / actix-web (`apps/`) |
|---|---|---|
| minimal | `zapline new -t minimal`: `GET /` and `GET /hello/{name}` | the same two routes |
| api | `zapline new -t api`: `/health` and `/users` CRUD over an in-memory store, `Valid<Json<..>>` validation with field errors, the OpenAPI document with a docs UI, security headers, the dev console | the same routes and store, `validator` with a validating JSON extractor returning field errors, `utoipa` + `utoipa-scalar` for OpenAPI and a docs UI, security headers (tower-http `SetResponseHeader` / actix `DefaultHeaders`). No dev console: neither framework has one |

The zapline apps are scaffolded at run time from the CLI's templates, so they
are exactly what `zapline new` gives a user today, with the zapline
dependency pointing at this checkout and pinned with the workspace
`Cargo.lock`. The axum and actix-web apps have their own lockfiles.

The API apps are as close as the frameworks allow, not identical: zapline's
default features also compile in what the template doesn't use yet (query
and form extractors, layered config, tracing), much as a typical axum or
actix-web project adds `tracing`, `config` or `tower-http` features over
time. That is part of what "a scaffolded zapline app" costs, and the
minimal row shows the floor.

## What is measured

Each app gets an empty target directory. Dependencies are downloaded first
(`cargo fetch`), so network time is excluded.

| column | |
|---|---|
| crates compiled | `Compiling` lines in the cold build: the app plus its dependency graph |
| cold check | `cargo check` from scratch; best of `COLD_ROUNDS` (3) |
| cold build | `cargo build` (dev profile) from scratch; best of `COLD_ROUNDS` |
| rebuild after editing a handler | change a string literal in one handler (`src/lib.rs` for minimal, `src/routes/health.rs` for api), `cargo build`; median of `ROUNDS` |
| check after editing a handler | the same edit, `cargo check`; median of `ROUNDS` |
| debug binary | size of the dev-profile executable |

Cold runs set `CARGO_INCREMENTAL=0`. Cargo builds local path dependencies
incrementally, and the zapline apps depend on this checkout by path, which
would make them pay for incremental compilation that a crates.io dependency
never does; with it off, all three frameworks' dependencies build the same
way. Rebuilds use Cargo's default dev profile (incremental on for the app
crate), as in day-to-day work.

## Caveats

- The numbers are wall-clock times on whatever machine ran the script. They
  move with CPU count, disk, linker and other load (hence best-of-N for the
  cold runs, which are the most exposed to it); `results.md` records the
  machine, the toolchain and the load average before and after. Compare the
  rows of one run with each other, not runs on different machines.
- The default linker was used. `mold` or `lld` shortens the rebuild column
  for every framework, mostly for the largest binaries.
- Release builds (fat LTO) are not measured here; they are dominated by LTO
  and codegen of the whole graph rather than by the framework's API style.
