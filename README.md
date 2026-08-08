# grove

**Stop paying cold cargo for every branch and every agent.**

Grove keeps one warm `target/` per repo and toolchain, and lets every branch,
worktree, and coding agent build into it. It also scopes `cargo check` to the
packages your git diff actually touched. It is not a faster rustc, a remote
cache, or an sccache replacement.

```sh
cargo install --git https://github.com/thomgardiner/grove
cd your-repo
grove warm     # fill the shared target once
grove check    # check only what your dirty files touch
```

## Why

Cargo keys build fingerprints to absolute paths, so every worktree gets its own
cold `target/` — and even pointing `CARGO_TARGET_DIR` at a shared directory
recompiles all path crates per worktree, because the package IDs differ. Grove
keeps one warm **seed** target for the primary checkout and gives each worktree
its own **lane**: a target directory hardlinked from the seed, so dependency
artifacts arrive already built and only your workspace crates recompile. Lanes
are independent, so agents run cargo in parallel without fighting over one
target lock.

## Benchmarks

Public repos, pinned commits, raw TSVs committed, rerunnable:
`./benchmark/prove_it.sh <repo>`. Apple M2 Max, 32 GB, APFS SSD, macOS 26.5.1,
rustc 1.97.1, no `RUSTC_WRAPPER`. Ranges are 3 samples, fresh worktree each.
Full tables and method: [`benchmark/RESULTS.md`](benchmark/RESULTS.md).

Grove helps when the package you check drags a heavy path-dependency tree.
**ripgrep** (`3fce3b5`, checking the bin, which depends on 9 sibling crates;
ranges are 3 samples):

| Scenario | grove | bare cargo | warm sccache |
| --- | ---: | ---: | ---: |
| Clean tree, "did anything change?" | 0.012–0.028 s | 0.054–0.057 s | n/a |
| Fresh worktree `cargo check -p`, cold target | 0.64–0.91 s | 3.02–3.33 s | 3.49–3.57 s |
| Fresh worktree `-p`, shared `CARGO_TARGET_DIR` (fair baseline) | 0.64–0.91 s | 0.96–0.99 s | n/a |
| Two agents checking concurrently, both done | 1.27–4.25 s | 1.61–1.65 s (shared) | n/a |
| Touch a file, recheck | 0.57 s | 0.46 s (`-p`) | n/a |

Warm local sccache does not fix the cross-worktree problem (it also keys on
absolute paths); grove does. The concurrent row has an asterisk: grove won 2 of
3 samples, with one 4.25 s stall on the shared-target lock.

Grove does not help when it doesn't. On **tokio** (`dd344a5`), `check -p tokio`
with default features compiles so little that grove's per-worktree sync
overhead dominates: plain shared `CARGO_TARGET_DIR` beats grove 0.61–0.81 s vs
0.97–1.21 s, and two concurrent agents finish in 1.02–1.16 s bare vs
2.04–2.14 s grove. On a single tiny crate (anyhow) grove loses outright:
nothing to reuse, pure overhead. If you already know `-p pkg` in one checkout,
bare cargo is slightly faster. All the loss tables are in RESULTS.md next to
the win.

## Use

```sh
grove warm                  # once per repo + toolchain
grove check                 # edit loop: dirty packages only
grove check --base auto     # verify: everything changed vs trunk (for PRs / handoff)
eval "$(grove env)"         # point your shell + rust-analyzer at the shared target
```

`--base auto` is the "am I actually done" command: it diffs against your trunk
branch and checks every package that changed, with `--locked`. Plain
`grove check` only covers uncommitted edits — don't hand off a branch on that
alone.

Also: `build` `clippy` `test` (same scoping), `exec -- <any cargo cmd>`,
`status`, `plan` (print what would be checked), `clean`.

## Worktrees and agents

```sh
path=$(grove worktree acquire --agent alice)   # creates ../your-repo-worktrees/alice
cd "$path" && grove check                      # builds into the shared warm target
grove check --base auto                        # before handoff
grove worktree release "$path"                 # --force discards dirty
```

Worktrees are created next to the repo so `path = "../sibling"` dependencies
still resolve.

Running a fleet of coding agents: `grove warm` once on the primary checkout,
then each agent acquires its own worktree. `grove skill install` writes a
SKILL.md into Claude/Codex-style harness directories so agents learn these
commands; skip it if you don't run agents.

`grove env` points bare cargo and rust-analyzer at this checkout's lane so IDE
builds share the same target directory.

## Layout

```text
$GROVE_CACHE/<repo-slug>/<toolchain>/seed/target/        # warm seed (primary)
$GROVE_CACHE/<repo-slug>/<toolchain>/lanes/<id>/target/  # worktree lanes
<repo-parent>/<repo>-worktrees/<agent>/                  # git worktrees (source)
```

`$GROVE_CACHE` defaults to `~/Library/Caches/grove` (macOS) or
`~/.cache/grove`. Keep it on the same disk as your repos — a slow external
cache volume erases the wins (~13× slower warm in a spot-check).

| Env | |
| --- | --- |
| `GROVE_CACHE` | cache root |
| `GROVE_WORKTREE_ROOT` | worktree parent (default: repo-adjacent) |
| `GROVE_CACHE_MAX_GB` | cache ceiling (default **40**; `0` = unlimited) |
| `GROVE_LANE_TTL_DAYS` | idle lanes on *other* hosts (default **1**; `0` = off) |

After every successful build, Grove drops agent lanes that are not live worktrees
and evicts cold caches until the root fits under `GROVE_CACHE_MAX_GB`. The seed for
the checkout you just built is kept; cold seeds from other repos go first.

Grove never sets `RUSTC_WRAPPER` and never forces incremental when a wrapper is
already set. Manual seed wipe still needs `grove clean --seed --force`. Orphan
cleanup only touches directories grove itself stamped.

## Library

```rust
use grove::{CacheHost, WorktreeManager, cargo_check};

let host = CacheHost::open(repo)?;
host.warm(repo)?;
cargo_check(repo, "HEAD", None)?;   // dirty-scoped
cargo_check(repo, "auto", None)?;   // vs trunk
```

Exit codes: `0` success (including "nothing to check"), `1`–`127` passed through
from cargo, `2` grove error.

## Not this tool

Task running, build farming, remote caching (sccache), container builds
(cargo-chef). Local warm-target reuse and git-scoped checks only.
