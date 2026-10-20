# zapline-cli

The `zapline` command: project scaffolding, code generators, a live-reload dev
server, route and OpenAPI inspection, benchmarking and toolchain checks.

```sh
cargo install zapline-cli      # installs `zapline` and `cargo-zapline`
zapline new my-api             # or: cargo zapline new my-api
cd my-api
zapline dev
```

## Commands

| Command | What it does |
|---|---|
| `zapline new <name> [-t minimal\|api\|full] [--path DIR] [--zapline-path PATH] [--no-git]` | Scaffold a project |
| `zapline generate resource <name> [--plural WORD]` (alias `g`) | Model, CRUD handlers, in-memory store and tests, mounted automatically |
| `zapline generate handler <name> [-m get] [-p /path]` | One route in `src/routes/<name>.rs` |
| `zapline generate middleware <name>` | A middleware function in `src/middleware/<name>.rs` |
| `zapline dev [--bin X] [--release] [-- <app args>]` | Rebuild and restart on file changes |
| `zapline routes [--bin X] [--json]` | Print the route table |
| `zapline openapi [-o openapi.json] [--bin X]` | Export the OpenAPI document |
| `zapline doctor` | Check rustc/cargo (>= 1.88), rustfmt, clippy, git, docker, wrk |
| `zapline bench <url> [-c 256] [-d 10s] [-t N] [--raw]` | Load test with `wrk` |

Global option: `-C <dir>` runs a command as if started in `<dir>`.
Colour is only used on a TTY and is disabled by `NO_COLOR`.

## Templates

- **minimal**: one route and an in-process test.
- **api** (default): routes, models, validation, OpenAPI docs and Swagger UI,
  Dockerfile (multi-stage, non-root), GitHub Actions CI, `.env.example`.
- **full**: everything in `api` plus layered config (`config/*.toml`), a
  dependency-injected repository, health probes, Prometheus metrics, tracing,
  security headers, request IDs, CORS, rate limiting, compression, panic
  catching, a graceful-shutdown hook and a background task.

Every project is a library plus a thin binary, so tests can build the same
`App` in-process (`app().test()`).

## How generation works

`generate resource post` writes `src/routes/posts.rs` and `tests/posts.rs`, then
registers the module in `src/routes/mod.rs` between the marker comments:

```rust
pub mod posts;

pub fn install(mut app: App) -> App {
    // zapline:routes:start
    app = app.plugin(posts::mount);
    // zapline:routes:end
    app
}
```

If the markers are missing, nothing is edited and you get copy-paste
instructions. Generation never overwrites existing files without `--force`, and
every conflict is detected before anything is written. New files are formatted
with `rustfmt` when it is installed.

## How `routes` and `openapi` work

The CLI builds the app, then runs it with an environment variable that makes
`App::run` do one job and exit:

- `ZAPLINE_PRINT_ROUTES=1`: print `[{"method":"GET","path":"/x"}, ...]`
- `ZAPLINE_EXPORT_OPENAPI=<path>`: write the OpenAPI document

The app gets 60 seconds to finish, so an app that ignores the variable cannot
hang the CLI. The OpenAPI file is written to a temporary name first and only
moved into place once it has been validated.

## Development

```sh
cargo test -p zapline-cli                                  # fast suite
cargo test -p zapline-cli --test scaffold -- --ignored     # compiles generated projects
```

The slow `scaffold` tests generate each template with a path dependency on this
checkout, then build, test, format-check and lint the result. Set
`ZAPLINE_SCAFFOLD_ZAPLINE` to test against another checkout.
