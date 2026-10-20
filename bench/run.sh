#!/usr/bin/env bash
# Compare zapline, axum and actix-web on identical endpoints.
#
#   ./bench/run.sh            # defaults below
#   DURATION=30s CONNS=512 ./bench/run.sh
#
# Servers are pinned to SERVER_CPUS and wrk to CLIENT_CPUS so the load
# generator does not steal CPU from the server under test.
set -euo pipefail
cd "$(dirname "$0")/.."

DURATION=${DURATION:-10s}
CONNS=${CONNS:-256}
SERVER_CPUS=${SERVER_CPUS:-0,1}
CLIENT_CPUS=${CLIENT_CPUS:-2,3}
WORKERS=${WORKERS:-2}
WRK_THREADS=${WRK_THREADS:-2}
ROUNDS=${ROUNDS:-3}
OUT=${OUT:-bench/results.md}

cargo build --release -q -p bench-servers

declare -A PORTS=([zapline]=3001 [axum]=3002 [actix]=3003)
FRAMEWORKS=(zapline axum actix)
TESTS=("plaintext:/plaintext:" "json:/json:" "path-param:/user/42:" "plaintext-pipelined-16:/plaintext:pipeline")

rps() { grep "Requests/sec" | awk '{print $2}'; }
lat() { grep -E "^\s+Latency\s+[0-9]" | awk '{print $2}'; }

declare -A RESULT
for fw in "${FRAMEWORKS[@]}"; do
  port=${PORTS[$fw]}
  WORKERS=$WORKERS taskset -c "$SERVER_CPUS" "target/release/$fw" >/dev/null 2>&1 &
  pid=$!
  for _ in $(seq 50); do curl -sf "http://127.0.0.1:$port/plaintext" >/dev/null && break; sleep 0.1; done

  for t in "${TESTS[@]}"; do
    IFS=: read -r name path mode <<<"$t"
    url="http://127.0.0.1:$port$path"
    args=(-t"$WRK_THREADS" -c"$CONNS" --latency)
    [[ $mode == pipeline ]] && extra=(-s bench/pipeline.lua "$url" -- 16) || extra=("$url")
    taskset -c "$CLIENT_CPUS" wrk "${args[@]}" -d3s "${extra[@]}" >/dev/null   # warm-up
    best=0; best_lat=""
    for _ in $(seq "$ROUNDS"); do
      out=$(taskset -c "$CLIENT_CPUS" wrk "${args[@]}" -d"$DURATION" "${extra[@]}")
      r=$(echo "$out" | rps); l=$(echo "$out" | lat)
      if (( $(echo "$r > $best" | bc -l) )); then best=$r; best_lat=$l; fi
    done
    RESULT[$fw,$name]="$best|$best_lat"
    printf "%-8s %-24s %12s req/s  avg latency %s\n" "$fw" "$name" "$best" "$best_lat"
  done
  kill "$pid"; wait "$pid" 2>/dev/null || true
done

{
  echo "| test | zapline | axum | actix-web |"
  echo "|---|---:|---:|---:|"
  for t in "${TESTS[@]}"; do
    name=${t%%:*}
    row="| $name"
    for fw in "${FRAMEWORKS[@]}"; do
      IFS='|' read -r r l <<<"${RESULT[$fw,$name]}"
      row+=" | $(printf "%'.0f" "$r") req/s ($l)"
    done
    echo "$row |"
  done
  echo
  echo "_${WORKERS} server workers on CPUs ${SERVER_CPUS}; wrk ${WRK_THREADS} threads / ${CONNS} connections on CPUs ${CLIENT_CPUS}; best of ${ROUNDS} × ${DURATION}; $(nproc) CPUs total, $(uname -sr)._"
} >"$OUT"
echo; cat "$OUT"
