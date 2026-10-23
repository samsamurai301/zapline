#!/usr/bin/env bash
# Create frameworks/Rust/zapline/ in a FrameworkBenchmarks checkout (or any
# directory), ready for `./tfb --mode verify --test zapline` and a pull
# request to TechEmpower.
#
#   ./bench/tfb/prepare-submission.sh ~/FrameworkBenchmarks              # zapline from git (this commit)
#   ZAPLINE_DEP=crates ./bench/tfb/prepare-submission.sh ~/FrameworkBenchmarks
#   ZAPLINE_DEP=vendor ./bench/tfb/prepare-submission.sh /tmp/zapline-tfb   # self-contained, for local docker builds
#
# ZAPLINE_DEP picks where the image gets zapline from:
#   git     (default) the GitHub repository at ZAPLINE_REV (default: this
#           checkout's HEAD, which must be pushed)
#   crates  crates.io, version ZAPLINE_VERSION (default: the workspace version)
#   vendor  `cargo package` output of crates/zapline in vendor/zapline, and
#           every other dependency `cargo vendor`ed into
#           vendor/crates: the image builds with no network at all. TFB
#           prefers a published dependency, so use it to test the image
#           rather than to submit
set -euo pipefail
TFB=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$TFB/../.." && pwd)
[ $# -eq 1 ] || { echo "usage: $0 <FrameworkBenchmarks checkout | output directory>" >&2; exit 2; }

if [ -d "$1/frameworks/Rust" ]; then DEST=$1/frameworks/Rust/zapline; else DEST=$1; fi
MODE=${ZAPLINE_DEP:-git}
REPO=${ZAPLINE_GIT:-https://github.com/samsamurai301/zapline}
VERSION=${ZAPLINE_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}
FEATURES='default-features = false, features = ["json", "postgres", "views", "cache"]'

case $MODE in
  git)
    REV=${ZAPLINE_REV:-$(git -C "$ROOT" rev-parse HEAD)}
    DEP="zapline = { git = \"$REPO\", rev = \"$REV\", $FEATURES }" ;;
  crates)
    DEP="zapline = { version = \"=$VERSION\", $FEATURES }" ;;
  vendor)
    DEP="zapline = { path = \"vendor/zapline\", $FEATURES }" ;;
  *) echo "error: ZAPLINE_DEP must be git, crates or vendor" >&2; exit 2 ;;
esac

rm -rf "$DEST"
mkdir -p "$DEST"
cp -r "$TFB/src" "$TFB/templates" "$TFB/benchmark_config.json" "$TFB/config.toml" \
  "$TFB/zapline.dockerfile" "$TFB/Cargo.lock" "$DEST/"
printf 'target\n' >"$DEST/.dockerignore"

# The manifest, with the zapline dependency swapped and the local-only
# comments dropped.
# In vendor mode, keep vendor/zapline out of the workspace: as a member it
# would have all of its optional dependencies resolved.
awk -v dep="$DEP" -v vendor="$([ "$MODE" = vendor ] && echo 1)" '
  /^zapline = / { print dep; next }
  /^\[workspace\]$/ && vendor { print; print "exclude = [\"vendor\"]"; next }
  /^# Not part of the zapline workspace/ { skip = 1 }
  skip && /^#/ { next }
  { skip = 0; print }
' "$TFB/Cargo.toml" >"$DEST/Cargo.toml"

if [ "$MODE" = vendor ]; then
  pkg=$(mktemp -d)
  cargo package -q -p zapline --no-verify --allow-dirty --manifest-path "$ROOT/Cargo.toml" \
    --target-dir "$pkg/target"
  mkdir -p "$DEST/vendor"
  tar -xzf "$pkg/target/package/zapline-$VERSION.crate" -C "$DEST/vendor"
  mv "$DEST/vendor/zapline-$VERSION" "$DEST/vendor/zapline"
  rm -rf "$pkg"
fi

# README for the TFB repository.
cat >"$DEST/README.md" <<EOF
# zapline

[zapline](https://github.com/samsamurai301/zapline) is a batteries-included
Rust web framework: thread-per-core Tokio runtimes with \`SO_REUSEPORT\`,
typed extractors, OpenAPI, sqlx databases and MiniJinja views.

This implementation uses the framework as an application would: \`App\`
routes, extractors, \`zapline::cache::Cache\` for cached queries and
\`zapline::view::Views\` (MiniJinja) for fortunes. Database access is sqlx
against Postgres, with one connection pool per worker thread so a request
and its queries stay on one core.

## Test URLs

| test | url |
|---|---|
| JSON | http://localhost:8080/json |
| Plaintext | http://localhost:8080/plaintext |
| Single query | http://localhost:8080/db |
| Multiple queries | http://localhost:8080/queries?queries=20 |
| Fortunes | http://localhost:8080/fortunes |
| Updates | http://localhost:8080/updates?queries=20 |
| Cached queries | http://localhost:8080/cached-queries?count=100 |
EOF

# Refresh the lockfile for the new zapline source; everything else keeps
# the versions zapline is tested with.
if ! (cd "$DEST" && cargo metadata --format-version 1 >/dev/null 2>"$DEST/.metadata-error"); then
  echo "warning: could not resolve zapline from $MODE:" >&2
  sed 's/^/  /' "$DEST/.metadata-error" >&2
  [ "$MODE" = git ] && echo "  push $REV to $REPO (or set ZAPLINE_REV to a pushed commit), then run 'cargo update -p zapline' in $DEST" >&2
fi
rm -f "$DEST/.metadata-error"

if [ "$MODE" = vendor ]; then
  mkdir -p "$DEST/.cargo"
  (cd "$DEST" && cargo vendor --locked vendor/crates 2>/dev/null >.cargo/config.toml)
fi
echo "wrote $DEST (zapline from $MODE)"
