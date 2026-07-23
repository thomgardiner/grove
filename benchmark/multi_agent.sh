#!/usr/bin/env sh
# G2 multi-agent bench pack: N concurrent worktrees, wall clock + disk,
# behavior-equivalence against cargo-default (SHA-256 probe receipt).
#
# Defaults favor a reproducible laptop pack on the synthetic medium fixture.
# Override any env var the same way as head_to_head.mjs.
#
# Examples:
#   ./benchmark/multi_agent.sh
#   BENCH_FIXTURE=  CONCURRENCY=2 RUNS=3 ./benchmark/multi_agent.sh   # ripgrep (slow)
#   KEEP_ARTIFACTS=1 ./benchmark/multi_agent.sh
set -e
cd "$(dirname "$0")/.."

export BENCH_FIXTURE="${BENCH_FIXTURE-medium}"
export RUNS="${RUNS-3}"
export CONCURRENCY="${CONCURRENCY-4}"
# Seeded phase only admits cargo-default + grove (canonical CoW story). Add
# cargo-isolated/shared only with PHASES=cold,warm (no seeded).
export PHASES="${PHASES-cold,seeded,warm}"
export MODES="${MODES-cargo-default,grove}"
# Medium fixture is small; do not demand the default 30 GiB floor unless the
# operator raises it.
export BENCH_START_FREE_GB="${BENCH_START_FREE_GB-8}"

echo "grove multi-agent pack:"
echo "  fixture=${BENCH_FIXTURE:-ripgrep}"
echo "  runs=${RUNS} concurrency=${CONCURRENCY} phases=${PHASES}"
echo "  modes=${MODES}"
echo "  free floor GiB=${BENCH_START_FREE_GB}"

exec node benchmark/head_to_head.mjs "$@"
