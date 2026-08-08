# Benchmark results

Public repos, pinned commits, raw data committed under `benchmark/out/`. Rerun:

```sh
git clone https://github.com/BurntSushi/ripgrep && git -C ripgrep checkout 3fce3b5bb0236da2df6d99672afb8a719642eca7
git clone https://github.com/tokio-rs/tokio && git -C tokio checkout dd344a550c2c6ddc500a2a8ad2eca8e097795252
git clone https://github.com/dtolnay/anyhow && git -C anyhow checkout 30e1a6f73777a76bb7b39d91d7364b8186599343
./benchmark/prove_it.sh <repo> [label]
```

## Machine

| | |
| --- | --- |
| Hardware | Apple M2 Max, 12 cores, 32 GB RAM, internal APFS SSD |
| OS | macOS 26.5.1 |
| Toolchain | rustc 1.97.1, cargo 1.97.1 |
| Wrappers | none for grove/bare scenarios; sccache 0.16.0 in S8 only |
| Grove | 0.1.1 |

Repos, grove cache, and bare targets on the same internal SSD. An uncommitted
spot-check with the cache on an external USB volume penalized grove ~13× on
warm; keep cache and source on the same class of disk.

## Projects and sampled package

`prove_it.sh` checks one package per run (`-p`). Which package matters more
than repo size: grove's worktree reuse pays off only when that package pulls a
heavy path-dependency tree.

| Run | Repo | Commit | Packages | Rust LOC | `-p` sample | Path-dep weight |
| --- | --- | --- | ---: | ---: | --- | --- |
| `ripgrep` | ripgrep | `3fce3b5` | 11 | ~56k | `ripgrep` (the bin) | heavy: depends on 9 sibling crates |
| `tokio` | tokio | `dd344a5` | 10 | ~178k | `tests-integration` | light |
| `tokio-pkg` | tokio | `dd344a5` | 10 | ~178k | `tokio` (default features) | light: default features compile little |
| `anyhow` | anyhow | `30e1a6f` | 1 | ~5.9k | `anyhow` | none |

## Method

- Fresh isolated `GROVE_CACHE`, bare `CARGO_TARGET_DIR`, and `SCCACHE_DIR`
  per run, same SSD.
- Wall clock via bash `TIMEFORMAT=%R` around the whole process.
- S2, S5, S6, S7, S8 are 3 samples each (S5–S8 use fresh worktrees per
  sample); S1/S3/S4 are single runs; treat deltas under ~0.2 s as noise.
  Outliers are kept in the ranges, not trimmed.
- S5 bare = `cargo check -p` into a fresh empty target (cold agent).
- S6 bare = plain `git worktree` + already-warm shared `CARGO_TARGET_DIR`
  (the zero-tool incumbent). Grove's S6 column repeats its S5 samples, since
  grove behaves identically in both; only the bare baseline changes.
- S7 = two agents invoked simultaneously, wall clock until both finish.
- S8 = warm a local sccache from the primary checkout, then fresh plain
  worktree + empty target + `RUSTC_WRAPPER=sccache` (the wrapper-cache
  incumbent).

## ripgrep: the shape grove is for

| Scenario | grove | bare | sccache |
| --- | ---: | ---: | ---: |
| S1 cold fill | 7.54 s | 8.08 s | 3.97 s (`-p` only) |
| S2 hot no-op (3×) | 0.012–0.028 s | 0.054–0.057 s | n/a |
| S3 touch build.rs, recheck | 0.57 s | 0.46 s (`-p`) | n/a |
| S5 fresh worktree `-p` (3×) | 0.64–0.91 s | 3.02–3.33 s (empty target) | 3.49–3.57 s |
| S6 fresh worktree `-p` vs shared target (3×) | 0.64–0.91 s | 0.96–0.99 s | n/a |
| S7 two concurrent agents (3×) | 1.27–4.25 s | 1.61–1.65 s (shared) / 4.37–5.79 s (isolated) | n/a |

Checking the `ripgrep` bin rebuilds its 9 path crates under a plain shared
target or sccache (both key on each worktree's absolute path); grove's
stable-src keeps them warm. Warm sccache is no better than a cold target here.
S7: grove won 2 of 3 samples (1.27, 1.32) with one 4.25 s outlier: the
shared-target lock can stall a concurrent pair; more samples needed before
calling concurrent a reliable win.

## tokio: the shape grove is not for

| Scenario | grove | bare | sccache | Sample |
| --- | ---: | ---: | ---: | --- |
| S2 hot no-op (3×) | 0.016–0.057 s | 0.073–0.085 s | n/a | both runs |
| S5 fresh worktree `-p` (3×) | 0.97–1.21 s | 0.22–0.25 s (empty target) | 0.77–0.80 s | `tokio` |
| S6 fresh worktree vs shared target (3×) | 0.97–1.21 s | 0.61–0.81 s | n/a | `tokio` |
| S6 fresh worktree vs shared target (3×) | 1.04–1.18 s | 0.61–0.67 s | n/a | `tests-integration` |
| S7 two concurrent agents (3×) | 2.04–2.14 s | 1.02–1.16 s (shared) / 1.04–1.28 s (isolated) | n/a | `tokio` |
| S7 two concurrent agents (3×) | 2.19–2.26 s | 0.94–1.24 s (shared) / 4.77–5.32 s (isolated) | n/a | `tests-integration` |

`cargo check -p tokio` with default features compiles almost nothing, so
grove's per-worktree overhead (stable-src sync of a ~178k LOC checkout, plan)
dominates and **plain shared `CARGO_TARGET_DIR` beats grove** in every worktree
scenario here. Only the hot no-op stays a grove win.

## anyhow: single tiny crate

| Scenario | grove | bare | sccache |
| --- | ---: | ---: | ---: |
| S2 hot no-op (3×) | 0.010–0.014 s | 0.041–0.050 s | n/a |
| S5 fresh worktree `-p` (3×) | 0.64–0.70 s | 0.50–4.43 s (empty target; one outlier) | 0.75–0.78 s |
| S6 fresh worktree vs shared target (3×) | 0.64–0.70 s | 0.62–0.71 s | n/a |
| S7 two concurrent agents (3×) | 1.15–1.29 s | 0.73–0.84 s (shared) / 0.98–1.05 s (isolated) | n/a |

A loss everywhere except the no-op: with one crate and no path deps there is
nothing to reuse, so grove's worktree overhead is pure cost.

## What the data supports

1. **Hot no-op is 3–5× faster on warm samples** (10–57 ms vs 41–85 ms; the first sample after other work can narrow to ~1.5×). Matters for
   agents that poll "did anything change?"; humans won't feel it.
2. **Worktree reuse wins when the checked package has a heavy path-dep tree**
   (ripgrep bin: 0.64–0.91 s vs 0.96–0.99 s shared-target, 3.0–3.3 s cold,
   3.5–3.6 s warm sccache). sccache does not solve the cross-worktree path
   problem; grove does.
3. **Worktree reuse loses when it doesn't** (tokio, anyhow): stable-src sync
   overhead exceeds the recompile it avoids, and plain shared
   `CARGO_TARGET_DIR` is faster. Cold fill and known-`-p` rechecks favor bare.
4. **Concurrent agents are not yet a clean grove win even on ripgrep**: 2 of 3
   samples beat the bare-shared baseline, one stalled at 4.25 s on the target
   lock. On tokio-shaped repos, concurrent grove loses outright.

## Not measured

Linux, Windows, >2 concurrent agents, workspaces whose sampled package is both
large *and* path-dep-heavy at scale (the ripgrep shape at 500k+ LOC).

## Raw runs

Committed in this directory: `ripgrep-20260806T131444Z.{tsv,meta,md}`,
`tokio-20260806T131602Z.*`, `tokio-pkg-20260806T131728Z.*`,
`anyhow-20260806T131407Z.*`. The `.meta` files record the exact grove/rustc
versions and paths used.
