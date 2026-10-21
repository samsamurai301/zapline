# Contributing

Thanks for helping! zapline is developed and tested on Linux, macOS and Windows.

Run `./scripts/check.sh` (Linux/macOS) or `.\scripts\check.ps1` (Windows, PowerShell) for the full suite. Before you open a PR:

1. `cargo fmt --all`
2. `cargo clippy -p zapline -p zapline-macros --all-targets -- -D warnings`
3. `cargo test -p zapline`
4. If you touched the hot path (`server.rs`, `router.rs`, `app.rs`,
   `handler.rs`), run `./bench/run.sh` before and after, and paste both
   tables into the PR.

Performance changes need numbers. API changes need a test in
`crates/zapline/tests/` and a README update.
