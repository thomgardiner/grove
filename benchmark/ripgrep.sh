#!/usr/bin/env bash
# Copy-on-write clone microbenchmark on ripgrep (a real 61-crate dependency graph).
# This models Grove's seed primitive, but it does not invoke Grove or use fresh git
# worktrees. Use head_to_head.mjs for comparative claims.
#
# Method. The canonical is built once with the commands grove runs (`cargo check
# --workspace` then `cargo nextest run --workspace --no-run`) under grove's profile env
# (debug=0, split-debuginfo off).
#   COLD   = a fresh empty target + the same command + the same env — a fresh worktree
#            without grove.
#   SEEDED = clonefile the canonical into a fresh target (grove's seed) + the same
#            command — a fresh output tree with a manually cloned seed.
# Same env both sides, so the delta is purely the seed. A valid seed compiles ~0 crates
# yet still succeeds: proof Cargo accepts the cloned artifacts as fresh.
set -uo pipefail

BENCH="$(cd "$(dirname "$0")" && pwd)/.work"
REPO="$BENCH/ripgrep"
CANON="$BENCH/canon"
CARGO_ENV=(CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
  CARGO_PROFILE_DEV_SPLIT_DEBUGINFO=off CARGO_PROFILE_TEST_SPLIT_DEBUGINFO=off)

now()      { date +%s.%N; }
secs()     { awk "BEGIN{printf \"%.1f\", $2-$1}"; }
compiles() { grep -cE '^[[:space:]]*(Checking|Compiling)' "$1" 2>/dev/null | tr -d '\n'; }
gcargo()   { local d="$1"; shift
  env "${CARGO_ENV[@]}" CARGO_TARGET_DIR="$d/target" CARGO_BUILD_BUILD_DIR="$d/build" cargo "$@"; }

mkdir -p "$BENCH"
[ -d "$REPO" ] || git clone --depth 1 --branch 14.1.1 https://github.com/BurntSushi/ripgrep.git "$REPO"
cd "$REPO"
rm -rf "$CANON" "$BENCH/cold" "$BENCH/seeded" "$BENCH"/*.log
echo "==> pre-fetching dependencies (excluded from all timings)"
cargo fetch --locked >/dev/null 2>&1

echo "==> building the canonical once (grove's warm)"
mkdir -p "$CANON"; t0=$(now)
gcargo "$CANON" check --workspace --locked > "$BENCH/warm.log" 2>&1 || { tail -15 "$BENCH/warm.log"; exit 1; }
gcargo "$CANON" nextest run --workspace --no-run --locked >> "$BENCH/warm.log" 2>&1 || { tail -15 "$BENCH/warm.log"; exit 1; }
echo "    canonical warmed in $(secs "$t0" "$(now)")s"

scenario() { # $1=label  rest=cargo args
  local label="$1"; shift
  rm -rf "$BENCH/cold"; mkdir -p "$BENCH/cold"
  local c0; c0=$(now); gcargo "$BENCH/cold" "$@" > "$BENCH/cold.log" 2>&1; local crc=$? c1; c1=$(now)
  rm -rf "$BENCH/seeded"; mkdir -p "$BENCH/seeded"
  local s0; s0=$(now)
  cp -cR "$CANON/target" "$BENCH/seeded/target"; [ -d "$CANON/build" ] && cp -cR "$CANON/build" "$BENCH/seeded/build"
  local sm; sm=$(now); gcargo "$BENCH/seeded" "$@" > "$BENCH/seeded.log" 2>&1; local src=$? s1; s1=$(now)
  printf '  %-20s COLD %ss / %s crates (rc%s)   SEEDED %ss = clone %ss + build %ss / %s crates (rc%s)\n' \
    "$label" "$(secs "$c0" "$c1")" "$(compiles "$BENCH/cold.log")" "$crc" \
    "$(secs "$s0" "$s1")" "$(secs "$s0" "$sm")" "$(secs "$sm" "$s1")" "$(compiles "$BENCH/seeded.log")" "$src"
}

echo "==> cold vs seeded"
scenario "check --workspace"  check --workspace --locked
scenario "build test binaries" nextest run --workspace --no-run --locked
echo "==> canonical logical size: $(du -sh "$CANON" 2>/dev/null | cut -f1) (shared copy-on-write per seed)"
