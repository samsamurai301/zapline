#!/usr/bin/env bash
# Full local verification: what CI runs, in one command.
#
#   ./scripts/check.sh          # everything
#   ./scripts/check.sh quick    # fmt + clippy + tests (all features)
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

step "rustfmt"
cargo fmt --all --check

step "clippy (all features)"
cargo clippy --workspace --exclude bench-servers --all-features --all-targets -- -D warnings

step "tests (all features)"
cargo test -p zapline --all-features
cargo test -p zapline-cli

[[ "${1:-}" == "quick" ]] && exit 0

step "feature matrix"
cargo check -p zapline --no-default-features
for f in macros json query form openapi validate config tracing http2 tls ws fs multipart compression jwt cookies; do
  echo "  - $f"
  cargo check -q -p zapline --no-default-features --features "$f"
done
cargo check -p zapline

step "examples"
cargo build -p zapline-examples --examples

step "docs"
RUSTDOCFLAGS="-D warnings" cargo doc -p zapline --all-features --no-deps

echo; echo "All checks passed."
