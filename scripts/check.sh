#!/usr/bin/env bash
# Full local verification: what CI runs, in one command.
#
#   ./scripts/check.sh            # everything
#   ./scripts/check.sh quick      # fmt + clippy + tests (all features)
#   ./scripts/check.sh ecosystem  # everything, plus examples/ecosystem
#                                 # (sqlx, SeaORM, Diesel, Leptos: heavy deps)
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

step "rustfmt"
cargo fmt --all --check

step "clippy (all features)"
cargo clippy --workspace --exclude bench-servers --all-features --all-targets -- -D warnings

step "tests (all features)"
cargo test -p zapline --all-features
cargo test -p zapline-codegen
cargo test -p zapline-cli

[[ "${1:-}" == "quick" ]] && exit 0

step "feature matrix"
cargo check -p zapline --no-default-features
for f in macros json query form openapi validate config tracing http2 tls http3 ws fs multipart compression jwt jwks cookies views postgres sqlite password mail storage s3 cache oauth i18n redis redis-tls otlp tz chrono uuid; do
  echo "  - $f"
  cargo check -q -p zapline --no-default-features --features "$f"
done
cargo check -p zapline

step "examples"
cargo build -p zapline-examples --examples
# The flagship app is its own project (own lockfile, sqlite only).
(cd examples/saas && CARGO_TARGET_DIR="$PWD/../../target/saas" cargo test -q)

step "TechEmpower app"
# Its own project (TFB builds it alone); `./bench/tfb/run.sh` verifies it
# against Postgres.
(cd bench/tfb && cargo clippy -q --all-targets -- -D warnings && cargo test -q)

step "docs"
RUSTDOCFLAGS="-D warnings" cargo doc -p zapline --all-features --no-deps

if [[ "${1:-}" == "ecosystem" ]]; then
  step "ecosystem examples (sqlx, SeaORM, Diesel, Leptos)"
  # Their own workspace and lockfile, so their dependencies stay out of the
  # main build.
  (
    cd examples/ecosystem
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/../../target/ecosystem}"
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace -q
  )
fi

echo; echo "All checks passed."
