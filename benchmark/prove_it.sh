#!/usr/bin/env bash
# Prove-it: wall-clock Grove vs bare cargo (isolated caches).
# Usage: prove_it.sh <repo-path> [label]
# Requires: grove on PATH, cargo, git, python3, bash time
set -euo pipefail

REPO="${1:?repo path}"
LABEL="${2:-$(basename "$REPO")}"
REPO="$(cd "$REPO" && pwd)"
OUT_DIR="${GROVE_BENCH_OUT:-$(cd "$(dirname "$0")" && pwd)/out}"
mkdir -p "$OUT_DIR"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="$OUT_DIR/${LABEL}-${STAMP}.tsv"
SUMMARY="$OUT_DIR/${LABEL}-${STAMP}.md"
META="$OUT_DIR/${LABEL}-${STAMP}.meta"
GROVE_CACHE_BENCH="${GROVE_CACHE_BENCH:-$OUT_DIR/cache-${LABEL}-${STAMP}}"
BARE_TARGET="$OUT_DIR/bare-target-${LABEL}-${STAMP}"
WT_ROOT="$OUT_DIR/wts-${LABEL}-${STAMP}"

# Fairness: isolate from ambient sccache/incremental wrappers
unset RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER || true
export GROVE_CACHE="$GROVE_CACHE_BENCH"
export GROVE_WORKTREE_ROOT="$WT_ROOT"

secs() {
  TIMEFORMAT='%R'
  { time "$@" >/tmp/grove-bench-stdout.$$ 2>/tmp/grove-bench-stderr.$$; } 2>/tmp/grove-bench-time.$$
  local code=$?
  local t
  t=$(cat /tmp/grove-bench-time.$$)
  cat /tmp/grove-bench-stdout.$$
  cat /tmp/grove-bench-stderr.$$ >&2
  rm -f /tmp/grove-bench-stdout.$$ /tmp/grove-bench-stderr.$$ /tmp/grove-bench-time.$$
  printf '%s' "$t" > /tmp/grove-bench-last-time
  return $code
}

record() {
  local scenario="$1" tool="$2" code="$3" seconds="$4" note="${5:-}"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$LABEL" "$scenario" "$tool" "$code" "$seconds" "$note" | tee -a "$LOG"
}

echo -e "label\tscenario\ttool\texit\tseconds\tnote" > "$LOG"
cd "$REPO"

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "WARN: repo has dirty files; S2/S3 may not be pure no-op/touch" | tee -a "$META"
fi

PKG="$(cargo metadata --format-version 1 --no-deps 2>/dev/null | python3 -c '
import json,sys
m=json.load(sys.stdin)
members=set(m.get("workspace_members") or [])
cands=[]
for p in m["packages"]:
    if members and p["id"] not in members:
        continue
    cands.append(p["name"])
print(cands[-1] if cands else "unknown")
' 2>/dev/null || echo unknown)"
PKG="${GROVE_BENCH_PKG:-$PKG}"

TOUCH_FILE="$(python3 - <<'PY' "$REPO" "$PKG"
import json, os, subprocess, sys
repo, pkg = sys.argv[1], sys.argv[2]
skip = {"target", "archive", ".git", "benchmark", "out", "node_modules"}
meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=repo, text=True))
path = None
for p in meta["packages"]:
    if p["name"] != pkg:
        continue
    root = os.path.dirname(p["manifest_path"])
    for dirpath, dirnames, files in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in skip]
        if set(dirpath.split(os.sep)) & skip:
            continue
        for f in files:
            if f.endswith(".rs") and f != "main.rs":
                path = os.path.join(dirpath, f)
                break
        if path:
            break
    break
print(path or "")
PY
)"

if [[ -z "$TOUCH_FILE" || ! -f "$TOUCH_FILE" ]]; then
  TOUCH_FILE="$(find "$REPO" -name '*.rs' \
    -not -path '*/target/*' -not -path '*/.git/*' \
    | head -1)"
fi

{
  echo "repo=$REPO"
  echo "label=$LABEL"
  echo "stamp=$STAMP"
  echo "pkg=$PKG"
  echo "touch=$TOUCH_FILE"
  echo "grove=$(command -v grove)"
  echo "grove_version=$(grove --version 2>/dev/null || true)"
  echo "rustc=$(rustc -V 2>/dev/null || true)"
  echo "grove_cache=$GROVE_CACHE"
  echo "bare_target=$BARE_TARGET"
  echo "worktree_root=$WT_ROOT"
  echo "RUSTC_WRAPPER=${RUSTC_WRAPPER:-<unset>}"
} | tee "$META"

rm -rf "$GROVE_CACHE" "$BARE_TARGET" "$WT_ROOT"
mkdir -p "$BARE_TARGET" "$WT_ROOT"

# ========== S1: cold fill ==========
echo "=== S1 cold: grove warm ==="
set +e
secs grove warm
c=$?; t=$(cat /tmp/grove-bench-last-time)
set -e
record "S1_cold_warm" "grove" "$c" "$t" "check --workspace into seed"

echo "=== S1 cold: bare cargo check --workspace ==="
set +e
secs env CARGO_TARGET_DIR="$BARE_TARGET" cargo check --workspace
c=$?; t=$(cat /tmp/grove-bench-last-time)
set -e
record "S1_cold_workspace" "bare" "$c" "$t" "isolated CARGO_TARGET_DIR"

# ========== S2: hot no-op (3 runs each) ==========
for i in 1 2 3; do
  echo "=== S2 hot no-op grove check run $i ==="
  set +e
  secs grove check --base HEAD
  c=$?; t=$(cat /tmp/grove-bench-last-time)
  set -e
  record "S2_hot_noop_r${i}" "grove" "$c" "$t" "base HEAD dirty-only"

  echo "=== S2 hot no-op bare workspace run $i ==="
  set +e
  secs env CARGO_TARGET_DIR="$BARE_TARGET" cargo check --workspace
  c=$?; t=$(cat /tmp/grove-bench-last-time)
  set -e
  record "S2_hot_noop_r${i}" "bare" "$c" "$t" "workspace hot"
done

# ========== S3: one-file dirty impact (independent dirty cycles) ==========
# Each tool gets a restore+re-touch so bare workspace is not helped by a prior bare -p.
s3_cycle() {
  local name="$1"
  shift
  cp "$TOUCH_FILE" /tmp/grove-bench-touch.bak.$$
  printf '\n// grove-bench-touch %s %s\n' "$STAMP" "$name" >> "$TOUCH_FILE"
  set +e
  secs "$@"
  local c=$? t
  t=$(cat /tmp/grove-bench-last-time)
  set -e
  mv /tmp/grove-bench-touch.bak.$$ "$TOUCH_FILE"
  # leave tree clean for next cycle
  printf '%s' "$c" > /tmp/grove-bench-last-code
  printf '%s' "$t" > /tmp/grove-bench-last-time
}

echo "=== S3 grove check (impact HEAD) ==="
s3_cycle impact grove check --base HEAD
record "S3_touch_impact" "grove" "$(cat /tmp/grove-bench-last-code)" "$(cat /tmp/grove-bench-last-time)" \
  "touch $(basename "$TOUCH_FILE") → plan → -p"

echo "=== S3 bare cargo check -p $PKG ==="
s3_cycle minus_p env CARGO_TARGET_DIR="$BARE_TARGET" cargo check -p "$PKG"
record "S3_touch_minus_p" "bare" "$(cat /tmp/grove-bench-last-code)" "$(cat /tmp/grove-bench-last-time)" \
  "user already knows package"

echo "=== S3 bare cargo check --workspace ==="
s3_cycle workspace env CARGO_TARGET_DIR="$BARE_TARGET" cargo check --workspace
record "S3_touch_workspace" "bare" "$(cat /tmp/grove-bench-last-code)" "$(cat /tmp/grove-bench-last-time)" \
  "full workspace after independent touch"

# ========== S4: first worktree after warm seed (hardlink path) ==========
AGENT="bench-${LABEL}-$$"
echo "=== S4 worktree acquire + check -p $PKG ==="
cd "$REPO"
set +e
WT_PATH=$(grove worktree acquire --agent "$AGENT" 2>/tmp/grove-wt-err.$$)
acq_code=$?
set -e
if [[ $acq_code -ne 0 || -z "${WT_PATH:-}" || ! -d "$WT_PATH" ]]; then
  record "S4_worktree_check" "grove" "$acq_code" "0" "acquire failed: $(tr '\n' ' ' </tmp/grove-wt-err.$$)"
else
  cd "$WT_PATH"
  set +e
  secs grove check -p "$PKG"
  c=$?; t=$(cat /tmp/grove-bench-last-time)
  set -e
  record "S4_worktree_first" "grove" "$c" "$t" "after warm seed; expect seed hardlink"

  # ========== S5: fresh grove worktree, first visit — 3 samples ==========
  for i in 1 2 3; do
    cd "$REPO"
    AGENT2="bench-s5r${i}-${LABEL}-$$"
    set +e
    WT2=$(grove worktree acquire --agent "$AGENT2" 2>/tmp/grove-wt2-err.$$)
    set -e
    if [[ -n "${WT2:-}" && -d "$WT2" ]]; then
      cd "$WT2"
      set +e
      secs grove check -p "$PKG"
      c=$?; t=$(cat /tmp/grove-bench-last-time)
      set -e
      record "S5_worktree_fresh_r${i}" "grove" "$c" "$t" "fresh worktree first visit; seed warm"
      cd "$REPO"
      grove worktree release "$WT2" --force 2>/dev/null || true
    fi
  done

  # bare cold agent baseline: -p into a new empty target — 3 samples
  for i in 1 2 3; do
    BARE_WT="$OUT_DIR/bare-wt-${LABEL}-${STAMP}-r${i}"
    rm -rf "$BARE_WT"
    cd "$REPO"
    set +e
    secs env CARGO_TARGET_DIR="$BARE_WT" cargo check -p "$PKG"
    c=$?; t=$(cat /tmp/grove-bench-last-time)
    set -e
    record "S5_bare_fresh_target_r${i}" "bare" "$c" "$t" "new empty target_dir -p (cold agent proxy)"
    rm -rf "$BARE_WT"
  done

  # S6: the zero-tool incumbent — plain git worktree + shared CARGO_TARGET_DIR,
  # fresh worktree per sample (path PackageIds differ per worktree) — 3 samples
  for i in 1 2 3; do
    SHARED_WT="$OUT_DIR/shared-wt-${LABEL}-${STAMP}-r${i}"
    cd "$REPO"
    rm -rf "$SHARED_WT"
    if git worktree add --detach "$SHARED_WT" >/dev/null 2>&1; then
      cd "$SHARED_WT"
      set +e
      secs env CARGO_TARGET_DIR="$BARE_TARGET" cargo check -p "$PKG"
      c=$?; t=$(cat /tmp/grove-bench-last-time)
      set -e
      record "S6_bare_shared_target_r${i}" "bare" "$c" "$t" "plain git worktree + warm shared CARGO_TARGET_DIR"
      cd "$REPO"
      git worktree remove --force "$SHARED_WT" >/dev/null 2>&1 || true
    else
      record "S6_bare_shared_target_r${i}" "bare" "1" "0" "git worktree add failed"
    fi
  done

  # ========== S7: two agents concurrently, wall clock to both done (3 samples) ==========
  for i in 1 2 3; do
    # (a) grove: two fresh worktrees into the shared seed target (cargo serializes)
    cd "$REPO"
    set +e
    WTA=$(grove worktree acquire --agent "bench-s7a${i}-${LABEL}-$$" 2>/dev/null)
    WTB=$(grove worktree acquire --agent "bench-s7b${i}-${LABEL}-$$" 2>/dev/null)
    set -e
    if [[ -n "${WTA:-}" && -d "$WTA" && -n "${WTB:-}" && -d "$WTB" ]]; then
      set +e
      secs bash -c "(cd '$WTA' && grove check -p '$PKG' >/dev/null 2>&1) & (cd '$WTB' && grove check -p '$PKG' >/dev/null 2>&1) & wait"
      c=$?; t=$(cat /tmp/grove-bench-last-time)
      set -e
      record "S7_concurrent_grove_shared_r${i}" "grove" "$c" "$t" "2 fresh worktrees, shared seed target, parallel invocation"
      cd "$REPO"
      grove worktree release "$WTA" --force 2>/dev/null || true
      grove worktree release "$WTB" --force 2>/dev/null || true
    fi

    # (b) bare: two plain worktrees + shared warm CARGO_TARGET_DIR, parallel
    CWA="$OUT_DIR/s7-shared-a-${LABEL}-${STAMP}-r${i}"; CWB="$OUT_DIR/s7-shared-b-${LABEL}-${STAMP}-r${i}"
    cd "$REPO"; rm -rf "$CWA" "$CWB"
    if git worktree add --detach "$CWA" >/dev/null 2>&1 && git worktree add --detach "$CWB" >/dev/null 2>&1; then
      set +e
      secs bash -c "(cd '$CWA' && CARGO_TARGET_DIR='$BARE_TARGET' cargo check -p '$PKG' >/dev/null 2>&1) & (cd '$CWB' && CARGO_TARGET_DIR='$BARE_TARGET' cargo check -p '$PKG' >/dev/null 2>&1) & wait"
      c=$?; t=$(cat /tmp/grove-bench-last-time)
      set -e
      record "S7_concurrent_bare_shared_r${i}" "bare" "$c" "$t" "2 plain worktrees, shared warm target, parallel (cargo lock serializes)"
      cd "$REPO"
      git worktree remove --force "$CWA" >/dev/null 2>&1 || true
      git worktree remove --force "$CWB" >/dev/null 2>&1 || true
    fi

    # (c) bare: two plain worktrees, isolated cold targets, truly parallel
    IWA="$OUT_DIR/s7-iso-a-${LABEL}-${STAMP}-r${i}"; IWB="$OUT_DIR/s7-iso-b-${LABEL}-${STAMP}-r${i}"
    cd "$REPO"; rm -rf "$IWA" "$IWB"
    if git worktree add --detach "$IWA" >/dev/null 2>&1 && git worktree add --detach "$IWB" >/dev/null 2>&1; then
      set +e
      secs bash -c "(cd '$IWA' && CARGO_TARGET_DIR='$IWA-target' cargo check -p '$PKG' >/dev/null 2>&1) & (cd '$IWB' && CARGO_TARGET_DIR='$IWB-target' cargo check -p '$PKG' >/dev/null 2>&1) & wait"
      c=$?; t=$(cat /tmp/grove-bench-last-time)
      set -e
      record "S7_concurrent_bare_isolated_r${i}" "bare" "$c" "$t" "2 plain worktrees, per-worktree cold targets, parallel"
      cd "$REPO"
      git worktree remove --force "$IWA" >/dev/null 2>&1 || true
      git worktree remove --force "$IWB" >/dev/null 2>&1 || true
      rm -rf "$IWA-target" "$IWB-target"
    fi
  done

  # ========== S8: sccache incumbent (if installed) ==========
  # Warm a local sccache from the primary checkout, then fresh plain worktree +
  # empty target + RUSTC_WRAPPER=sccache: the wrapper-cache alternative.
  if command -v sccache >/dev/null 2>&1; then
    export SCCACHE_DIR="$OUT_DIR/sccache-${LABEL}-${STAMP}"
    sccache --stop-server >/dev/null 2>&1 || true
    rm -rf "$SCCACHE_DIR"
    SCC_WARM="$OUT_DIR/sccache-warm-target-${LABEL}-${STAMP}"
    rm -rf "$SCC_WARM"
    cd "$REPO"
    set +e
    secs env RUSTC_WRAPPER=sccache CARGO_TARGET_DIR="$SCC_WARM" cargo check -p "$PKG"
    c=$?; t=$(cat /tmp/grove-bench-last-time)
    set -e
    record "S8_sccache_warm_fill" "sccache" "$c" "$t" "primary checkout, empty target, cold sccache"
    for i in 1 2 3; do
      SWT="$OUT_DIR/s8-wt-${LABEL}-${STAMP}-r${i}"
      cd "$REPO"; rm -rf "$SWT"
      if git worktree add --detach "$SWT" >/dev/null 2>&1; then
        cd "$SWT"
        set +e
        secs env RUSTC_WRAPPER=sccache CARGO_TARGET_DIR="$SWT-target" cargo check -p "$PKG"
        c=$?; t=$(cat /tmp/grove-bench-last-time)
        set -e
        record "S8_sccache_worktree_r${i}" "sccache" "$c" "$t" "fresh plain worktree, empty target, warm local sccache"
        cd "$REPO"
        git worktree remove --force "$SWT" >/dev/null 2>&1 || true
        rm -rf "$SWT-target"
      fi
    done
    sccache --stop-server >/dev/null 2>&1 || true
    unset SCCACHE_DIR
    rm -rf "$SCC_WARM"
  fi

  cd "$REPO"
  grove worktree release "$WT_PATH" --force 2>/dev/null || true
fi
rm -f /tmp/grove-wt-err.$$ /tmp/grove-wt2-err.$$

# ========== summary ==========
python3 - <<'PY' "$LOG" "$SUMMARY" "$META" "$LABEL" "$REPO" "$STAMP" "$PKG" "$TOUCH_FILE"
import sys
from pathlib import Path
log, summary, meta, label, repo, stamp, pkg, touch = sys.argv[1:]
rows = []
for line in Path(log).read_text().splitlines()[1:]:
    if not line.strip():
        continue
    lab, sc, tool, code, sec, *rest = line.split("\t")
    note = rest[0] if rest else ""
    try:
        sec_f = float(sec)
    except ValueError:
        sec_f = None
    rows.append((sc, tool, code, sec, sec_f, note))

def picks(prefix, tool=None):
    out = []
    for r in rows:
        if r[0].startswith(prefix) and (tool is None or r[1] == tool):
            out.append(r)
    return out

def min_sec(rs):
    vals = [r[4] for r in rs if r[4] is not None]
    return min(vals) if vals else None

lines = []
lines.append(f"# Prove-it: {label}")
lines.append("")
lines.append(f"- **Repo:** `{repo}`")
lines.append(f"- **When (UTC):** {stamp}")
lines.append(f"- **Package sample:** `{pkg}`")
lines.append(f"- **Touch file:** `{touch}`")
lines.append(f"- **Meta:** `{meta}`")
lines.append("")
lines.append("## Method")
lines.append("")
lines.append("- Isolated `GROVE_CACHE` and bare `CARGO_TARGET_DIR` (neither is the developer’s real `./target`).")
lines.append("- `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` unset for the run.")
lines.append("- Wall clock via bash `TIMEFORMAT=%R` around the full process.")
lines.append("- S2 repeated 3×; table shows all runs; summary uses min where noted.")
lines.append("- S4/S5 use `grove check -p <pkg>` so worktree path is not a dirty-only no-op.")
lines.append("- S5 bare baseline = `cargo check -p` into a **fresh empty** target (cold agent proxy), not the already-warm bare workspace target.")
lines.append("- S6 = plain `git worktree` + warm shared `CARGO_TARGET_DIR` (the zero-tool incumbent).")
lines.append("")
lines.append("| Scenario | Tool | Exit | Seconds | Note |")
lines.append("| --- | --- | --- | --- | --- |")
for sc, tool, code, sec, _, note in rows:
    lines.append(f"| {sc} | {tool} | {code} | {sec} | {note} |")
lines.append("")
lines.append("## Highlights (this run only)")
lines.append("")
g1 = next((r for r in rows if r[0] == "S1_cold_warm" and r[1] == "grove"), None)
b1 = next((r for r in rows if r[0] == "S1_cold_workspace" and r[1] == "bare"), None)
if g1 and b1 and g1[4] is not None and b1[4] is not None:
    lines.append(f"- **S1 cold:** grove warm `{g1[3]}s` vs bare workspace `{b1[3]}s` (expect same order of magnitude; not the win).")
g2 = min_sec(picks("S2_hot_noop", "grove"))
b2 = min_sec(picks("S2_hot_noop", "bare"))
if g2 is not None and b2 is not None:
    lines.append(f"- **S2 hot no-op (min of 3):** grove `{g2:.3f}s` vs bare workspace `{b2:.3f}s`.")
g3 = next((r for r in rows if r[0] == "S3_touch_impact"), None)
b3p = next((r for r in rows if r[0] == "S3_touch_minus_p"), None)
b3w = next((r for r in rows if r[0] == "S3_touch_workspace"), None)
if g3 and b3p and b3w:
    lines.append(f"- **S3 one-file touch:** grove impact `{g3[3]}s` vs bare `-p` `{b3p[3]}s` vs bare workspace `{b3w[3]}s`.")
g4 = next((r for r in rows if r[0] == "S4_worktree_first"), None)
if g4:
    lines.append(f"- **S4 first worktree `-p` after seed warm:** grove `{g4[3]}s`.")
def rng(prefix, tool):
    vals = sorted(r[4] for r in picks(prefix, tool) if r[4] is not None)
    return f"{vals[0]:.3f}–{vals[-1]:.3f}s" if vals else None
g5 = rng("S5_worktree_fresh", "grove"); b5 = rng("S5_bare_fresh_target", "bare"); b6 = rng("S6_bare_shared_target", "bare")
if g5:
    lines.append(f"- **S5 fresh worktree `-p` (3 samples):** grove `{g5}` vs bare empty target `{b5}` vs shared target `{b6}`.")
for sc, tool7, label7 in [("S7_concurrent_grove_shared", "grove", "grove shared"), ("S7_concurrent_bare_shared", "bare", "bare shared CTD"), ("S7_concurrent_bare_isolated", "bare", "bare isolated cold")]:
    v7 = rng(sc, tool7)
    if v7:
        lines.append(f"- **S7 two concurrent agents ({label7}, 3 samples):** `{v7}` to both done.")
s8 = rng("S8_sccache_worktree", "sccache")
if s8:
    lines.append(f"- **S8 fresh worktree + empty target + warm sccache (3 samples):** `{s8}`.")
lines.append("")
lines.append("## Reading")
lines.append("")
lines.append("- **S1** proves cold fill is not free; Grove is not claiming faster first compile than cargo.")
lines.append("- **S2** is the edit-loop claim: clean tree + `HEAD` should skip work; bare still invokes cargo workspace.")
lines.append("- **S3** is impact routing vs knowing `-p` vs full workspace (Grove should beat workspace; vs bare `-p` should be close).")
lines.append("- **S4/S5** is the multi-agent claim: after seed warm, worktree `-p` should not look like a full cold S1.")
lines.append("")
lines.append(f"Raw TSV: `{log}`")
Path(summary).write_text("\n".join(lines) + "\n")
print(f"WROTE {summary}")
print(Path(summary).read_text())
PY
