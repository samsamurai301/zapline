#!/usr/bin/env bash
# Hostile-traffic, chaos and soak tests against a real zapline server.
#
#   ./bench/chaos/run.sh                 # every scenario, 60s soak
#   ./bench/chaos/run.sh soak=600        # 10 minute soak
#   ./bench/chaos/run.sh slowloris h2    # only scenarios whose key contains these
#
# Linux only (it reads /proc for memory, file descriptors and CPU time).
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --bins -q
# The attacks open thousands of sockets.
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)" || true
exec ./target/release/zapline-chaos "$@"
