#!/usr/bin/env bash
# TechEmpower-style comparison of zapline, axum and actix-web, run entirely in
# Docker (Linux containers), so it works the same on Linux, macOS and Windows
# (Git Bash with Docker Desktop):
#
#   ./bench/tfb/run.sh
#   FRAMEWORKS="zapline axum" TESTS="json db" DURATION=30s ./bench/tfb/run.sh
#
# The server, Postgres and wrk run with host networking on disjoint CPU sets.
# Every framework serves the same endpoints with the same database code
# (bench/servers/src/lib.rs); see bench/tfb/README.md for the methodology.
set -euo pipefail
export MSYS_NO_PATHCONV=1 # Git Bash: pass container paths through untouched
cd "$(dirname "$0")/../.."
ROOT=$(pwd -W 2>/dev/null || pwd)

FRAMEWORKS=${FRAMEWORKS:-"zapline axum actix"}
TESTS=${TESTS:-"plaintext json path-param plaintext-pipelined db queries fortunes updates"}
DURATION=${DURATION:-15s}
WARMUP=${WARMUP:-5s}
ROUNDS=${ROUNDS:-1}
CONNS=${CONNS:-512}
PIPE_CONNS=${PIPE_CONNS:-1024}
PIPE_DEPTH=${PIPE_DEPTH:-16}
QUERIES=${QUERIES:-20}
WORKERS=${WORKERS:-4}
POOL=${POOL:-64}
SERVER_CPUS=${SERVER_CPUS:-0-3}
CLIENT_CPUS=${CLIENT_CPUS:-4-11}
DB_CPUS=${DB_CPUS:-12-19}
WRK_THREADS=${WRK_THREADS:-8}
RUST_IMAGE=${RUST_IMAGE:-rust:1-bookworm}
OUT=${OUT:-bench/tfb/results.md}
RAW=${RAW:-target/tfb}

mkdir -p "$RAW"
declare -A PORTS=([zapline]=3001 [axum]=3002 [actix]=3003)
DB_URL=postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1:55432/hello_world

log() { printf '\033[1m==> %s\033[0m\n' "$*" >&2; }

cleanup() { docker rm -f zb-server zb-postgres >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup

# ---- images and binaries ------------------------------------------------------
log "building images"
docker build -q -t zapline-bench-postgres bench/tfb/postgres >/dev/null
docker build -q -t zapline-bench-wrk -f bench/tfb/wrk/Dockerfile bench >/dev/null
docker volume create zapline-bench-target >/dev/null
docker volume create zapline-bench-cargo >/dev/null

log "building servers (release, fat LTO) with $RUST_IMAGE"
docker run --rm -v "$ROOT":/src -v zapline-bench-target:/target \
  -v zapline-bench-cargo:/usr/local/cargo/registry -w /src -e CARGO_TARGET_DIR=/target \
  "$RUST_IMAGE" cargo build --release -q -p bench-servers
RUSTC=$(docker run --rm "$RUST_IMAGE" rustc -V)

# ---- database -----------------------------------------------------------------
log "starting Postgres on CPUs $DB_CPUS"
docker run -d --name zb-postgres --network host --cpuset-cpus "$DB_CPUS" --shm-size 1g \
  zapline-bench-postgres >/dev/null
# The init scripts run on a socket-only temporary server first; TCP answers
# once the real one is up with the data loaded.
for i in $(seq 240); do
  docker exec zb-postgres psql -qtA -U benchmarkdbuser -h 127.0.0.1 -p 55432 -d hello_world \
    -c "SELECT count(*) FROM world" 2>/dev/null | grep -q 10000 && break
  if ((i == 240)) || [[ $(docker inspect -f '{{.State.Running}}' zb-postgres) != true ]]; then
    docker logs zb-postgres 2>&1 | tail -20 >&2
    log "Postgres did not come up"; exit 1
  fi
  sleep 0.5
done
PG_VERSION=$(docker exec zb-postgres postgres --version)

# ---- helpers ------------------------------------------------------------------
path_of() {
  case $1 in
    plaintext | plaintext-pipelined) echo /plaintext ;;
    json) echo /json ;;
    path-param) echo /user/42 ;;
    db) echo /db ;;
    queries) echo "/queries?queries=$QUERIES" ;;
    fortunes) echo /fortunes ;;
    updates) echo "/updates?queries=$QUERIES" ;;
  esac
}

# Check a response the way TechEmpower's verifier would, loosely.
verify() {
  local fw=$1 test=$2 url=$3 body
  body=$(docker exec zb-server curl -sf "$url") || { echo "  $fw $test: request failed" >&2; return 1; }
  case $test in
    plaintext*) [[ $body == "Hello, World!" ]] ;;
    json) [[ $body == '{"message":"Hello, World!"}' ]] ;;
    path-param) [[ $body == '{"id":42,"name":"ada"}' ]] ;;
    db) [[ $body =~ ^\{\"id\":[0-9]+,\"randomNumber\":[0-9]+\}$ ]] ;;
    queries | updates) [[ $(grep -o '"randomNumber"' <<<"$body" | wc -l) -eq $QUERIES ]] ;;
    fortunes) [[ $body == *"&lt;script&gt;"* && $body == *"Additional fortune added at request time."* &&
      $(grep -o '<tr>' <<<"$body" | wc -l) -eq 14 ]] ;;
  esac || { echo "  $fw $test: unexpected response: ${body:0:200}" >&2; return 1; }
}

# wrk prints us/ms/s; normalize to milliseconds.
to_ms() { awk -v v="$1" 'BEGIN {
  n = v + 0; if (v ~ /us$/) n /= 1000; else if (v ~ /ms$/) ; else if (v ~ /s$/) n *= 1000; printf "%.2f", n }'; }

# Parse a wrk report into "rps avg p50 p99 errors".
parse_wrk() {
  awk '
    /Requests\/sec/ { rps = $2 }
    /^ +Latency +[0-9]/ { avg = $2 }
    /^ +50%/ { p50 = $2 }
    /^ +99%/ { p99 = $2 }
    /Non-2xx/ { err += $NF }
    /Socket errors/ { gsub(/,/, ""); err += $4 + $6 + $8 + $10 }
    END { printf "%s %s %s %s %d\n", rps, avg, p50, p99, err }'
}

wrk_run() { # duration url [pipelined]
  local args=(--rm --network host --cpuset-cpus "$CLIENT_CPUS" zapline-bench-wrk
    -t"$WRK_THREADS" --latency -d"$1" -H 'Connection: keep-alive')
  if [[ ${3:-} == pipelined ]]; then
    docker run "${args[@]}" -c"$PIPE_CONNS" -s /pipeline.lua "$2" -- "$PIPE_DEPTH"
  else
    docker run "${args[@]}" -c"$CONNS" "$2"
  fi
}

rss_mb() { # VmRSS or VmHWM of the server process, in MB
  docker exec zb-server awk -v k="$1:" '$1 == k { printf "%.1f", $2 / 1024 }' /proc/1/status
}

# ---- run ----------------------------------------------------------------------
declare -A RESULT IDLE PEAK
for fw in $FRAMEWORKS; do
  port=${PORTS[$fw]}
  log "$fw: starting on CPUs $SERVER_CPUS ($WORKERS workers, pool $POOL)"
  docker run -d --name zb-server --network host --cpuset-cpus "$SERVER_CPUS" \
    -e WORKERS="$WORKERS" -e POOL="$POOL" -e DATABASE_URL="$DB_URL" \
    -v zapline-bench-target:/target:ro "$RUST_IMAGE" "/target/release/$fw" >/dev/null
  for _ in $(seq 100); do
    docker exec zb-server curl -sf "http://127.0.0.1:$port/plaintext" >/dev/null 2>&1 && break
    sleep 0.2
  done
  IDLE[$fw]=$(rss_mb VmRSS)

  for test in $TESTS; do
    url="http://127.0.0.1:$port$(path_of "$test")"
    mode=; [[ $test == plaintext-pipelined ]] && mode=pipelined
    if ! verify "$fw" "$test" "$url"; then RESULT[$fw,$test]="fail"; continue; fi
    wrk_run "$WARMUP" "$url" $mode >/dev/null
    best=
    for r in $(seq "$ROUNDS"); do
      out=$(wrk_run "$DURATION" "$url" $mode)
      echo "$out" >"$RAW/$fw-$test-$r.txt"
      line=$(parse_wrk <<<"$out")
      if [[ -z $best ]] || awk -v a="${line%% *}" -v b="${best%% *}" 'BEGIN { exit !(a > b) }'; then
        best=$line
      fi
    done
    read -r rps avg p50 p99 err <<<"$best"
    rss=$(rss_mb VmRSS)
    RESULT[$fw,$test]="$rps $(to_ms "$avg") $(to_ms "$p50") $(to_ms "$p99") $err $rss"
    printf '%-8s %-20s %12.0f req/s  avg %7s ms  p99 %8s ms  errors %s  rss %s MB\n' \
      "$fw" "$test" "$rps" "$(to_ms "$avg")" "$(to_ms "$p99")" "$err" "$rss" >&2
  done
  PEAK[$fw]=$(rss_mb VmHWM)
  docker rm -f zb-server >/dev/null
done

# ---- report -------------------------------------------------------------------
fws=($FRAMEWORKS)
{
  echo "### Throughput (requests/second, higher is better)"
  echo
  printf '| test |'; for fw in "${fws[@]}"; do printf ' %s |' "$fw"; done; echo
  printf '|---|'; for _ in "${fws[@]}"; do printf -- '---:|'; done; echo
  for test in $TESTS; do
    printf '| %s |' "$test"
    top=0 topfw=
    for fw in "${fws[@]}"; do
      r=${RESULT[$fw,$test]:-fail}; [[ $r == fail ]] && continue
      if awk -v a="${r%% *}" -v b="$top" 'BEGIN { exit !(a > b) }'; then top=${r%% *} topfw=$fw; fi
    done
    for fw in "${fws[@]}"; do
      r=${RESULT[$fw,$test]:-fail}
      if [[ $r == fail ]]; then printf ' failed |'; continue; fi
      read -r rps _ _ _ err _ <<<"$r"
      cell=$(awk -v r="$rps" -v t="$top" 'BEGIN { printf "%\047.0f (%.0f%%)", r, 100 * r / t }')
      [[ $fw == "$topfw" ]] && cell="**$cell**"
      (( err > 0 )) && cell="$cell, $err errors"
      printf ' %s |' "$cell"
    done
    echo
  done
  echo
  echo "### Latency under that load (p50 / p99 ms, lower is better)"
  echo
  printf '| test |'; for fw in "${fws[@]}"; do printf ' %s |' "$fw"; done; echo
  printf '|---|'; for _ in "${fws[@]}"; do printf -- '---:|'; done; echo
  for test in $TESTS; do
    printf '| %s |' "$test"
    for fw in "${fws[@]}"; do
      r=${RESULT[$fw,$test]:-fail}
      if [[ $r == fail ]]; then printf ' failed |'; continue; fi
      read -r _ _ p50 p99 _ _ <<<"$r"
      printf ' %s / %s |' "$p50" "$p99"
    done
    echo
  done
  echo
  echo "### Memory (resident set size, MB, lower is better)"
  echo
  printf '| |'; for fw in "${fws[@]}"; do printf ' %s |' "$fw"; done; echo
  printf '|---|'; for _ in "${fws[@]}"; do printf -- '---:|'; done; echo
  printf '| idle, after startup |'; for fw in "${fws[@]}"; do printf ' %s |' "${IDLE[$fw]}"; done; echo
  printf '| peak, across all tests |'; for fw in "${fws[@]}"; do printf ' %s |' "${PEAK[$fw]}"; done; echo
  echo
  echo "_${WORKERS} server workers on CPUs ${SERVER_CPUS}, Postgres on CPUs ${DB_CPUS}, wrk ${WRK_THREADS} threads on CPUs ${CLIENT_CPUS};" \
    "${CONNS} connections (pipelined: ${PIPE_CONNS} × depth ${PIPE_DEPTH}); queries/updates at ${QUERIES} queries;" \
    "pool ${POOL}; best of ${ROUNDS} × ${DURATION} after a ${WARMUP} warm-up._"
  echo "_${RUSTC}; ${PG_VERSION}; $(docker info --format '{{.OperatingSystem}}, kernel {{.KernelVersion}}, {{.NCPU}} CPUs'); $(docker run --rm debian:bookworm-slim sh -c "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //'")._"
} >"$OUT"
echo >&2
cat "$OUT"
