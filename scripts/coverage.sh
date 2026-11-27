#!/usr/bin/env bash
# Line coverage of the framework and CLI tests (all features), failing when
# it drops under the floor in .github/coverage-floor.
#
#   ./scripts/coverage.sh          # needs: cargo install cargo-llvm-cov
#
# Writes target/lcov.info (for editors and coverage services) and prints a
# per-file summary. The floor only goes up: when coverage rises, raise the
# number in .github/coverage-floor in the same pull request. CI refuses a
# pull request that lowers it.
set -euo pipefail
cd "$(dirname "$0")/.."

floor=$(tr -d '[:space:]' < .github/coverage-floor)
cargo llvm-cov --all-features \
    -p zapline -p zapline-macros -p zapline-codegen -p zapline-cli \
    --lcov --output-path target/lcov.info \
    --fail-under-lines "$floor"
cargo llvm-cov report --summary-only
echo "line coverage is at or above the floor of $floor%"
