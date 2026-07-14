# grove

A build cache and worktree manager for Rust repos that several coding agents work on at
once. Each git worktree gets its own build dir, seeded copy-on-write from one shared warm
build, so a fresh worktree doesn't recompile from scratch. grove also hands out worktrees
and cleans up the ones agents abandon. Single binary, no config.

## Install

```sh
cargo install --git https://github.com/thomgardiner/grove
```

## Usage

```sh
grove cache warm       # build the shared canonical once, per repo
grove check            # check only what your git diff touched
grove test             # test only the affected packages

grove worktree acquire --agent alice   # fresh worktree on its own branch, prints the path
grove worktree reap                    # reclaim abandoned worktrees (work is salvaged first)
grove watch                            # daemon: prewarm new worktrees, reap dead ones

grove cache status     # disk and lanes
grove cache gc         # reclaim orphaned lanes, evict to the disk watermark
```

## How it works

- Each worktree gets an isolated, file-locked build dir (a lane) under `~/.cargo/grove`,
  so parallel agents never share a target directory.
- A cold lane is seeded from a warm canonical with one copy-on-write clone (APFS
  clonefile, ReFS block clone, btrfs/XFS reflink). Cargo rebuilds only what changed.
- The canonical is keyed by the repo's git dir, not `Cargo.lock`, so a dependency bump
  rebuilds a few crates instead of everything.
- `check`/`test` map the git diff to affected packages with `cargo metadata`.
- Disk is bounded by a free-space watermark plus LRU eviction of whole lanes.
- Worktrees grove creates all live in `~/.cargo/grove/worktrees/`, not scattered across
  your dev folder. Idle ones are reclaimed as builds run (or by `grove watch`); reap only
  touches worktrees grove leased, and salvages any uncommitted work to a branch first.

## Benchmarks

Cold build vs copy-on-write-seeded build, the same profile on both sides so the only
difference is the seed. macOS/APFS, Apple Silicon.

| Project | Task | Cold | Seeded | Speedup |
|---|---|---|---|---|
| ripgrep (61 crates) | `check --workspace` | 2.6s | 0.7s | 3.7x |
| ~210k-line workspace (570 crates) | `check --workspace` | 39.4s | 10.1s | 3.9x |
| ~210k-line workspace (570 crates) | `build --workspace --lib` | 51.5s | 15.0s | 3.4x |

The seeded build recompiles 0-1 of hundreds of crates; Cargo accepts the cloned artifacts
as fresh, and running the resulting test binaries gives the same results as a cold build.
Seeded time is the clone (1-2s) plus Cargo's own freshness scan of the graph, which is the
floor and grows with project size. So the ratio holds around 3-4x while the absolute time
saved scales up: a couple of seconds on ripgrep, 30-40s per fresh worktree on the large
one. A release build does far more codegen, so its cold time is higher and the gap wider.
Reproduce the ripgrep run with [`benchmark/ripgrep.sh`](benchmark/ripgrep.sh).

## License

MIT
