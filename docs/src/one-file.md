# One-file apps

A whole zapline app can live in a single `.rs` file, with no `Cargo.toml`, no
`src/` and no project directory. It is good for prototypes, internal tools,
webhooks, demos, bug reports and code an AI assistant hands you in one piece.

```sh
zapline new hello.rs      # write a starter: routes, a validated JSON model, OpenAPI docs, tests
zapline dev hello.rs      # serve on http://localhost:8080, rebuild and live-reload on every save
zapline run hello.rs      # build (cached) and run it
./hello.rs                # the same, through its shebang line
zapline test hello.rs     # run the #[cfg(test)] tests inside the file
```

The first build compiles zapline once. After that, every one-file app on the
machine shares those compiled dependencies, and running an unchanged file
starts it right away: cargo is not invoked at all.

## The file

```rust
#!/usr/bin/env -S zapline run
//! Hands out ids.
//!
//! ```cargo
//! [dependencies]
//! uuid = { version = "1", features = ["v4"] }
//! ```

use zapline::prelude::*;

#[get("/ids/new")]
async fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn main() -> std::io::Result<()> {
    App::new().mount(new_id).run("0.0.0.0:8080")
}
```

- The shebang line is optional. With it (and `chmod +x`), the file runs as
  `./app.rs`; everything after the file name goes to the app.
- Dependencies go in a `cargo` block inside the leading `//!` comment, in
  Cargo.toml syntax. The block is optional too.
- The rest is ordinary Rust. rustc ignores the shebang, and the header is a doc
  comment, so the file is still valid Rust that rustfmt and clippy accept.

## Dependencies

**zapline, serde, serde_json and tokio are always available**, so most apps need
no header at all:

| Crate | Default |
|---|---|
| `zapline` | this CLI's release series (e.g. `"0.1"`), default features |
| `serde` | `{ version = "1", features = ["derive"] }` |
| `serde_json` | `"1"` |
| `tokio` | `{ version = "1", features = ["full"] }` |

List one of them only to change it. An entry without a version, path or git
source keeps the default version, so `zapline = { features = ["ws", "sqlite"] }`
just adds features. Any other crate is added the usual way: `uuid = "1"`.

A header may contain these tables: `[package]` (only `edition`,
`rust-version`, `version`, `description`, `authors`, `license`),
`[dependencies]`, `[dev-dependencies]`, `[target.'cfg(...)'.dependencies]`,
`[features]`, `[profile.*]`, `[patch.*]` and `[lints]`. The package name comes
from the file name, and the file itself is the only target, so `[[bin]]`,
`[lib]`, `[workspace]` and build scripts are not allowed. The edition defaults
to 2021.

Relative `path = "..."` dependencies are relative to the file, not to the
hidden project, so a script next to a zapline checkout can use
`zapline = { path = "zapline/crates/zapline" }`. `zapline new hello.rs
--zapline-path <checkout>` writes that line for you.

## Commands

| Command | What it does |
|---|---|
| `zapline new <name>.rs [--path DIR]` | Write a starter (refuses to overwrite). On unix it is made executable |
| `zapline run [--release] <file.rs> [args]` | Build if needed, then run it in the current directory with `args` (a `--` before them is optional). Exits with the app's exit code |
| `zapline dev <file.rs> [-- args]` | Watch the file, rebuild after each save and swap in the new version with a browser reload, like `zapline dev` in a project (see [The dev loop](cli.md#the-dev-loop-zapline-dev)). Header changes take effect on save. A broken header or build keeps the previous version running |
| `zapline test <file.rs> [filter]` | Run the tests in the file and print only the failures and a summary |
| `zapline check [--clippy] <file.rs>` | One line per error or warning |
| `zapline routes [--json] <file.rs>` | Print the route table |
| `zapline run --print-manifest <file.rs>` | Write the hidden project and print the path of its `Cargo.toml` |

Options for `zapline run` come before the file; anything after the file belongs
to the app, so `./app.rs --release` passes `--release` to the app.

## Errors point at your file

A mistake in the header is reported at its line and column, like a compiler
error, and nothing is built:

```text
error: app.rs:7:15: invalid ```cargo block: invalid basic string, expected `"`
```

Compiler errors and warnings name the real file and line too, because the
generated project's binary target is your file itself, not a copy.
`include_str!("data.txt")` and `#[path]` modules resolve next to the file.

## How it works

`zapline run app.rs` turns the file into a cargo project in the user cache
directory and builds that:

```text
~/.cache/zapline/                       ($XDG_CACHE_HOME/zapline; %LOCALAPPDATA%\zapline on Windows)
├── scripts/<hash of the file's path>/
│   ├── Cargo.toml                      [[bin]] path = "/abs/path/app.rs", deps from the header
│   ├── Cargo.lock                      kept between runs: versions stay put until you change the header
│   └── .cargo/config.toml              build.target-dir = the shared target below
└── target/                             one target directory for every one-file app
```

- **One project per file path.** Edit the file and the same project is reused;
  move it and it gets a new one. The binary is named after the file plus a
  short hash, so `api.rs` files in different directories never overwrite each
  other in the shared target directory.
- **Shared target directory.** Dependencies are compiled once for all scripts
  (per version and feature set, as cargo does). `CARGO_TARGET_DIR` still wins
  when set.
- **No cargo for an unchanged file.** After a build, zapline records the
  executable together with a hash of the generated manifest. On the next run it
  compares the executable's age with every source file in the dep-info that
  cargo writes next to it (the script, files it includes, and the sources of
  path dependencies) and with the manifests of path dependencies. If nothing is
  newer, it starts the executable directly (a few milliseconds); otherwise it
  runs `cargo build`, which recompiles only what changed.
- On unix, `zapline run` replaces itself with the app (`exec`), so signals, the
  exit code and stdin are the app's own.

Delete `~/.cache/zapline` at any time to reclaim the space; it is rebuilt on
demand.

## Editor support

rust-analyzer needs a `Cargo.toml`. Point it at the hidden one:

```sh
zapline run --print-manifest app.rs
# ~/.cache/zapline/scripts/1f2e3d4c5b6a7980/Cargo.toml
```

and add that path to `rust-analyzer.linkedProjects` in your editor settings
(VS Code: `"rust-analyzer.linkedProjects": ["/home/you/.cache/zapline/scripts/.../Cargo.toml"]`).

## Growing into a project

When the file gets big, create a project with `zapline new my-app` and move the
code over: handlers and models go to `src/lib.rs` (or `src/routes/`), `main`
to `src/main.rs`, the header's dependencies to `Cargo.toml`, and the tests to
`tests/`. Everything else (the API, the test client, `zapline dev`) works the
same.
