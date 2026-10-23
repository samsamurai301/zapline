#!/usr/bin/env bash
# Compile-time benchmark: an app scaffolded with `zapline new` against the
# same app written with axum and with actix-web (apps/), at two sizes:
#
#   minimal  `zapline new -t minimal`: two routes          vs apps/{axum,actix}-minimal
#   api      `zapline new -t api`: /health + /users CRUD,  vs apps/{axum,actix}-api
#            validation, OpenAPI + docs UI, security headers  (validator, utoipa, utoipa-scalar)
#
# For each app, in its own empty target directory:
#   cold check    cargo check from scratch                         (best of COLD_ROUNDS)
#   cold build    cargo build (dev profile) from scratch           (best of COLD_ROUNDS)
#   rebuild       edit one handler's string literal, cargo build   (median of ROUNDS)
#   recheck       edit it again, cargo check                       (median of ROUNDS)
#
# Dependencies are downloaded before timing starts. Cold runs use
# CARGO_INCREMENTAL=0 so every app's dependencies build the same way (a
# `--zapline-path` dependency would otherwise be built incrementally, as a
# local crate, which a crates.io dependency is not); rebuilds use Cargo's
# default incremental dev profile, as in day-to-day work.
#
#   ./bench/compile/run.sh
#   ROUNDS=5 TIERS=api ./bench/compile/run.sh
#
# Environment: ROUNDS (3), COLD_ROUNDS (3), TIERS ("minimal api"), APPS (default: all three
# frameworks), WORK (scratch directory), JOBS (cargo -j), OUT.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
ROUNDS=${ROUNDS:-3}
COLD_ROUNDS=${COLD_ROUNDS:-3}
TIERS=${TIERS:-minimal api}
APPS=${APPS:-zapline axum actix}
WORK=${WORK:-${TMPDIR:-/tmp}/zapline-compile-bench}
OUT=${OUT:-$HERE/results.md}
JOBS_ARG=()
[ -n "${JOBS:-}" ] && JOBS_ARG=(-j "$JOBS")

# The CLI scaffolds the zapline apps; built with the caller's settings, not timed.
echo "== building the zapline CLI"
cargo build -q -p zapline-cli --manifest-path "$ROOT/Cargo.toml"
ZAPLINE=${CARGO_TARGET_DIR:-$ROOT/target}/debug/zapline

rm -rf "$WORK"
mkdir -p "$WORK"
for tier in $TIERS; do
  for fw in $APPS; do
    app=$fw-$tier
    if [ "$fw" = zapline ]; then
      "$ZAPLINE" new "$app" --template "$tier" --path "$WORK" --zapline-path "$ROOT/crates/zapline" --no-git >/dev/null 2>&1
      # The versions zapline itself is tested with.
      cp "$ROOT/Cargo.lock" "$WORK/$app/Cargo.lock"
    else
      cp -r "$HERE/apps/$app" "$WORK/$app"
    fi
  done
done

now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.1f", b - a }'; }
best() { tr ' ' '\n' | sort -g | head -1; }
median() { tr ' ' '\n' | sort -g | awk '{ v[NR] = $1 } END { print (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2 }'; }

# The handler each round edits, and how.
edit() { # dir tier round
  local file pattern replacement
  if [ "$2" = minimal ]; then
    file=$1/src/lib.rs pattern='Hello from [A-Za-z0-9_-]*!' replacement="Hello from edit-$3!"
  else
    file=$1/src/routes/health.rs pattern='status: "ok[a-z0-9]*"' replacement="status: \"ok$3\""
  fi
  grep -Eq "$pattern" "$file" || { echo "error: no handler to edit in $file" >&2; exit 1; }
  sed -E -i "s/$pattern/$replacement/" "$file"
  grep -q "$replacement" "$file"
}

declare -A R
load() { cut -d' ' -f1-3 /proc/loadavg; }
LOAD_START=$(load)
for tier in $TIERS; do
  for fw in $APPS; do
    app=$fw-$tier dir=$WORK/$app
    export CARGO_TARGET_DIR=$dir/target
    echo "== $app"
    (cd "$dir" && cargo fetch -q)

    cold_checks=() cold_builds=()
    for _ in $(seq "$COLD_ROUNDS"); do
      rm -rf "$CARGO_TARGET_DIR"
      t=$(now); (cd "$dir" && CARGO_INCREMENTAL=0 cargo check -q "${JOBS_ARG[@]}"); cold_checks+=("$(elapsed "$t")")
      rm -rf "$CARGO_TARGET_DIR"
      t=$(now)
      crates=$(cd "$dir" && CARGO_INCREMENTAL=0 cargo build "${JOBS_ARG[@]}" 2>&1 | grep -c '^ *Compiling ')
      cold_builds+=("$(elapsed "$t")")
    done
    cold_check=$(echo "${cold_checks[*]}" | best)
    cold_build=$(echo "${cold_builds[*]}" | best)

    # Rebuild once with incremental on (untimed): that's the state a developer is in.
    (cd "$dir" && cargo build -q "${JOBS_ARG[@]}")
    builds=() checks=()
    for i in $(seq "$ROUNDS"); do
      edit "$dir" "$tier" "b$i"
      t=$(now); (cd "$dir" && cargo build -q "${JOBS_ARG[@]}"); builds+=("$(elapsed "$t")")
    done
    (cd "$dir" && cargo check -q "${JOBS_ARG[@]}")
    for i in $(seq "$ROUNDS"); do
      edit "$dir" "$tier" "c$i"
      t=$(now); (cd "$dir" && cargo check -q "${JOBS_ARG[@]}"); checks+=("$(elapsed "$t")")
    done
    rebuild=$(echo "${builds[*]}" | median)
    recheck=$(echo "${checks[*]}" | median)
    size=$(du -k "$CARGO_TARGET_DIR/debug/$app" | cut -f1)
    R[$app]="$crates|$cold_check|$cold_build|$rebuild|$recheck|$((size / 1024))"
    printf "   %s crates, cold check %ss (%s), cold build %ss (%s), rebuild %ss (%s), recheck %ss (%s), debug binary %s MB\n" \
      "$crates" "$cold_check" "${cold_checks[*]}" "$cold_build" "${cold_builds[*]}" "$rebuild" "${builds[*]}" "$recheck" "${checks[*]}" "$((size / 1024))"
    rm -rf "$CARGO_TARGET_DIR"
  done
done
LOAD_END=$(load)

{
  echo "# Compile times: zapline vs axum vs actix-web"
  echo
  echo "Generated by \`bench/compile/run.sh\` on $(date -u +%Y-%m-%d). See [README.md](README.md) for what each app contains and how each step is measured."
  echo
  echo "| app | crates compiled | cold check | cold build | rebuild after editing a handler | check after editing a handler | debug binary |"
  echo "|---|---:|---:|---:|---:|---:|---:|"
  for tier in $TIERS; do
    for fw in $APPS; do
      app=$fw-$tier
      IFS='|' read -r crates cc cb rb rc size <<<"${R[$app]}"
      echo "| $app | $crates | ${cc} s | ${cb} s | ${rb} s | ${rc} s | ${size} MB |"
    done
  done
  echo
  echo "## Machine"
  echo
  echo "- $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //'), $(nproc) CPUs, $(free -g | awk '/Mem:/ {print $2}') GB RAM, $(uname -sr)"
  echo "- $(rustc --version), $(cargo --version | cut -d' ' -f1-2); dev profile (debug info on), default \`-j\`${JOBS:+ overridden to $JOBS}"
  echo "- Cold columns: best of ${COLD_ROUNDS} runs; rebuild and check columns: median of ${ROUNDS} edits"
  echo "- Load average (1/5/15 min): $LOAD_START at the start, $LOAD_END at the end"
} >"$OUT"
echo; cat "$OUT"
