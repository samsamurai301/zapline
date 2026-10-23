#!/usr/bin/env bash
# Run zapline's TechEmpower Framework Benchmarks implementation locally:
# start a Postgres cluster with TFB's schema and settings, build the app,
# check every endpoint with verify.py, then load each test with wrk the way
# TFB does and write results.md.
#
#   ./bench/tfb/run.sh                    # verify + benchmark
#   VERIFY_ONLY=1 ./bench/tfb/run.sh      # verify only
#   DURATION=30s CONNS=256 ./bench/tfb/run.sh
#
# Database tests run once per pool mode (POOLS):
#   per-worker        a pool per worker thread, no ping on checkout (the default)
#   per-worker-ping   the same, with sqlx's ping on every checkout (DB_PING=1)
#   shared            the canonical `App::database` + `Db` pool (sqlx defaults)
#
# Environment overrides: DURATION (15s, as TFB), WARMUP (5s), CONNS (512,
# TFB's top concurrency for database tests), WRK_THREADS, WORKERS,
# DB_POOL_SIZE, POOLS, SERVER_CPUS, PG_CPUS, CLIENT_CPUS, PG_BIN, PGDATA,
# PGPORT, PORT, OUT, VERIFY_ONLY, SKIP_VERIFY, KEEP_PG.
#
# Needs: a Postgres server install (initdb, pg_ctl, psql; contrib for
# pg_stat_statements), wrk, python3, curl, taskset. Linux only.
set -euo pipefail
TFB=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$TFB/../.." && pwd)

DURATION=${DURATION:-15s}
WARMUP=${WARMUP:-5s}
CONNS=${CONNS:-512}
POOLS=${POOLS:-per-worker per-worker-ping shared}
PORT=${PORT:-8080}
PGPORT=${PGPORT:-5433}
PGDATA=${PGDATA:-/var/tmp/zapline-tfb-pg}
OUT=${OUT:-$TFB/results.md}

# Split the machine: half for the app, a quarter each for Postgres and wrk.
NCPU=$(nproc)
cpus() { seq -s, "$1" "$2"; }
if [ "$NCPU" -ge 4 ]; then
  s=$((NCPU / 2)); p=$((NCPU / 4))
  DEFAULT_SERVER_CPUS=$(cpus 0 $((s - 1)))
  DEFAULT_PG_CPUS=$(cpus "$s" $((s + p - 1)))
  DEFAULT_CLIENT_CPUS=$(cpus $((s + p)) $((NCPU - 1)))
  DEFAULT_WORKERS=$s DEFAULT_WRK_THREADS=$((NCPU - s - p))
else
  echo "note: fewer than 4 CPUs, so the app, Postgres and wrk share them" >&2
  DEFAULT_SERVER_CPUS=$(cpus 0 $((NCPU - 1)))
  DEFAULT_PG_CPUS=$DEFAULT_SERVER_CPUS DEFAULT_CLIENT_CPUS=$DEFAULT_SERVER_CPUS
  DEFAULT_WORKERS=$NCPU DEFAULT_WRK_THREADS=1
fi
SERVER_CPUS=${SERVER_CPUS:-$DEFAULT_SERVER_CPUS}
PG_CPUS=${PG_CPUS:-$DEFAULT_PG_CPUS}
CLIENT_CPUS=${CLIENT_CPUS:-$DEFAULT_CLIENT_CPUS}
WORKERS=${WORKERS:-$DEFAULT_WORKERS}
WRK_THREADS=${WRK_THREADS:-$DEFAULT_WRK_THREADS}
DB_POOL_SIZE=${DB_POOL_SIZE:-16}

for tool in wrk python3 curl taskset; do
  command -v "$tool" >/dev/null || { echo "error: $tool is not installed (apt-get install wrk python3 curl util-linux)" >&2; exit 1; }
done

# ---------------------------------------------------------------------------
# Postgres: a private cluster with TFB's settings, so pg_stat_statements is
# loaded and nothing else on the machine is disturbed.

if [ -z "${PG_BIN:-}" ]; then
  PG_BIN=$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1)
  [ -n "$PG_BIN" ] || PG_BIN=$(dirname "$(command -v initdb || command -v pg_ctl || echo /usr/bin/x)")
fi
[ -x "$PG_BIN/pg_ctl" ] || { echo "error: no Postgres server found; install postgresql (+ contrib) or set PG_BIN" >&2; exit 1; }

# Postgres refuses to run as root.
if [ "$(id -u)" = 0 ]; then
  PG_USER=${PG_USER:-postgres}
  as_pg() { su "$PG_USER" -s /bin/bash -c "$(printf '%q ' "$@")"; }
else
  as_pg() { "$@"; }
fi
export PGOPTIONS="-c client_min_messages=warning"
PSQL=(psql -h "$PGDATA" -p "$PGPORT" -U postgres -X -q -v ON_ERROR_STOP=1)

if [ ! -s "$PGDATA/PG_VERSION" ]; then
  echo "== initdb $PGDATA"
  mkdir -p "$PGDATA"
  [ "$(id -u)" = 0 ] && chown "$PG_USER" "$PGDATA"
  as_pg "$PG_BIN/initdb" -D "$PGDATA" -U postgres --auth=trust -E UTF8 --locale=C >/dev/null
  echo "include_if_exists = 'tfb.conf'" >>"$PGDATA/postgresql.conf"
fi
cp "$TFB/postgresql.conf" "$PGDATA/tfb.conf"
[ "$(id -u)" = 0 ] && chown "$PG_USER" "$PGDATA/tfb.conf"

STARTED_PG=0
if ! as_pg "$PG_BIN/pg_ctl" -D "$PGDATA" status >/dev/null 2>&1; then
  echo "== starting Postgres on 127.0.0.1:$PGPORT (CPUs $PG_CPUS)"
  as_pg taskset -c "$PG_CPUS" "$PG_BIN/pg_ctl" -D "$PGDATA" -l "$PGDATA/server.log" -w \
    -o "-p $PGPORT -k $PGDATA -c listen_addresses=127.0.0.1" start >/dev/null
  STARTED_PG=1
fi

SERVER_PID=
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null
  if [ "$STARTED_PG" = 1 ] && [ -z "${KEEP_PG:-}" ]; then
    as_pg "$PG_BIN/pg_ctl" -D "$PGDATA" -m fast stop >/dev/null || true
  fi
}
trap cleanup EXIT

echo "== loading the TFB schema into hello_world"
"${PSQL[@]}" -d postgres <<'SQL'
SELECT 'CREATE ROLE benchmarkdbuser LOGIN PASSWORD ''benchmarkdbpass'''
  WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'benchmarkdbuser') \gexec
SELECT 'CREATE DATABASE hello_world OWNER benchmarkdbuser'
  WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'hello_world') \gexec
SQL
"${PSQL[@]}" -d hello_world -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements"
psql -h "$PGDATA" -p "$PGPORT" -U benchmarkdbuser -d hello_world -X -q -v ON_ERROR_STOP=1 \
  -f "$TFB/create-postgres.sql" 2>&1 | grep -v NOTICE || true
"${PSQL[@]}" -d hello_world -c "VACUUM ANALYZE"

# ---------------------------------------------------------------------------
# Build and run the app

echo "== building zapline-tfb (release)"
cargo build --release -q --manifest-path "$TFB/Cargo.toml"
BIN=${CARGO_TARGET_DIR:-$TFB/target}/release/zapline-tfb
BASE=http://127.0.0.1:$PORT
APP_LOG=${TMPDIR:-/tmp}/zapline-tfb.log

start_server() { # pool mode
  local pool=$1 ping=0
  [ "$pool" = per-worker-ping ] && pool=per-worker ping=1
  DATABASE_URL="postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1:$PGPORT/hello_world" \
    BIND="127.0.0.1:$PORT" WORKERS=$WORKERS DB_POOL=$pool DB_POOL_SIZE=$DB_POOL_SIZE DB_PING=$ping \
    TEMPLATES="$TFB/templates" taskset -c "$SERVER_CPUS" "$BIN" >"$APP_LOG" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 100); do curl -sf "$BASE/plaintext" >/dev/null && return; sleep 0.1; done
  echo "error: the app did not start:" >&2; cat "$APP_LOG" >&2; exit 1
}
stop_server() { kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=; }

VERIFY_PSQL="psql -h $PGDATA -p $PGPORT -U postgres -d hello_world"
if [ -z "${SKIP_VERIFY:-}" ]; then
  for mode in $POOLS; do
    echo "== verifying ($mode pool)"
    start_server "$mode"
    python3 "$TFB/verify.py" "$BASE" --psql "$VERIFY_PSQL"
    stop_server
  done
fi
[ -n "${VERIFY_ONLY:-}" ] && exit 0

# ---------------------------------------------------------------------------
# Load, as TFB does: wrk with keep-alive, an Accept header and an 8 s
# timeout; plaintext pipelines 16 requests per write.

ACCEPT_JSON="application/json,text/html;q=0.9,application/xhtml+xml;q=0.9,application/xml;q=0.8,*/*;q=0.7"
ACCEPT_HTML="text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
ACCEPT_TEXT="text/plain,text/html;q=0.9,application/xhtml+xml;q=0.9,application/xml;q=0.8,*/*;q=0.7"
# name|path|accept|pipeline|needs the database
TESTS=(
  "plaintext|/plaintext|$ACCEPT_TEXT|16|"
  "json|/json|$ACCEPT_JSON||"
  "db|/db|$ACCEPT_JSON||db"
  "queries (20)|/queries?queries=20|$ACCEPT_JSON||db"
  "fortunes|/fortunes|$ACCEPT_HTML||db"
  "updates (20)|/updates?queries=20|$ACCEPT_JSON||db"
  "cached-queries (100)|/cached-queries?count=100|$ACCEPT_JSON||"
)

declare -A RESULT
run_wrk() { # url accept pipeline duration
  local args=(-H "Host: 127.0.0.1" -H "Accept: $2" -H "Connection: keep-alive" --latency
    -d "$4" -c "$CONNS" --timeout 8 -t "$WRK_THREADS")
  if [ -n "$3" ]; then args+=(-s "$ROOT/bench/pipeline.lua" "$1" -- "$3"); else args+=("$1"); fi
  taskset -c "$CLIENT_CPUS" wrk "${args[@]}"
}

LOAD_START=$(cut -d' ' -f1-3 /proc/loadavg)
for mode in $POOLS; do
  echo "== benchmarking ($mode pool)"
  start_server "$mode"
  for t in "${TESTS[@]}"; do
    IFS='|' read -r name path accept pipeline needs_db <<<"$t"
    # Tests without the database only run once.
    [ -z "$needs_db" ] && [ "$mode" != "${POOLS%% *}" ] && continue
    run_wrk "$BASE$path" "$accept" "$pipeline" "$WARMUP" >/dev/null
    out=$(run_wrk "$BASE$path" "$accept" "$pipeline" "$DURATION")
    rps=$(awk '/Requests\/sec/ {print $2}' <<<"$out")
    lat=$(awk '/^ +Latency +[0-9]/ {print $2; exit}' <<<"$out")
    # wrk's latency distribution is meaningless with pipelining.
    p99=$(awk '/^ +99%/ {print $2}' <<<"$out"); [ -n "$pipeline" ] && p99=n/a
    errors=$(awk '/Non-2xx|Socket errors/ {print}' <<<"$out" | tr -s ' ' | paste -sd ';' -)
    RESULT[$mode,$name]="$(printf "%'.0f" "$rps") ($lat avg, $p99 p99)${errors:+ ERRORS: $errors}"
    printf "%-12s %-22s %12s req/s  avg %-8s p99 %s %s\n" "$mode" "$name" "$rps" "$lat" "$p99" "$errors"
  done
  stop_server
done

LOAD_END=$(cut -d' ' -f1-3 /proc/loadavg)

# ---------------------------------------------------------------------------
# Report

PG_VERSION=$("${PSQL[@]}" -d postgres -Atc "SHOW server_version")
{
  echo "# zapline TechEmpower results (local)"
  echo
  echo "Generated by \`bench/tfb/run.sh\` on $(date -u +%Y-%m-%d). Requests per second (average and p99 latency)."
  echo
  header="| test |"; sep="|---|"
  for mode in $POOLS; do header+=" \`$mode\` |"; sep+="---:|"; done
  echo "$header"; echo "$sep"
  for t in "${TESTS[@]}"; do
    IFS='|' read -r name _ _ _ needs_db <<<"$t"
    row="| $name |"
    for mode in $POOLS; do
      if [ -z "$needs_db" ] && [ "$mode" != "${POOLS%% *}" ]; then row+=" (no database) |"
      else row+=" ${RESULT[$mode,$name]:-} |"; fi
    done
    echo "$row"
  done
  echo
  echo "## Setup"
  echo
  echo "- Machine: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //'), $NCPU CPUs, $(free -g | awk '/Mem:/ {print $2}') GB RAM, $(uname -sr)"
  echo "- Pools: \`per-worker\` is one pool of $DB_POOL_SIZE connections per worker thread with no ping on checkout (the default); \`per-worker-ping\` adds sqlx's ping on every checkout; \`shared\` is \`App::database\` + \`Db\`: one pool of $((DB_POOL_SIZE * WORKERS)) connections opened on the startup runtime, with sqlx's defaults (ping included)"
  echo "- App: $WORKERS workers on CPUs $SERVER_CPUS; $(rustc --version)"
  echo "- Postgres $PG_VERSION on CPUs $PG_CPUS with TFB's postgresql.conf"
  echo "- $(wrk -v 2>&1 | head -1 | cut -d' ' -f1-2): $WRK_THREADS thread(s), $CONNS connections, on CPUs $CLIENT_CPUS; ${WARMUP} warm-up then one ${DURATION} run per test"
  echo "- Load average (1/5/15 min) $LOAD_START at the start, $LOAD_END at the end of the load runs"
  echo "- App, database and load generator share one machine, so compare columns with each other, not with published TFB rounds."
} >"$OUT"
echo; cat "$OUT"
