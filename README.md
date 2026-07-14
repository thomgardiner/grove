# grove

A build cache and worktree manager for Rust. Every git worktree gets its own build
directory, seeded copy-on-write from one shared warm build, so a fresh worktree builds in
seconds instead of recompiling from scratch. grove also runs the worktrees themselves:
handing them out on their own branches, tracking who is working on what so parallel work
does not collide, and reclaiming the ones that get abandoned (salvaging their work first).
One static binary that works with zero setup.

## Benchmarks

Cold build vs grove's copy-on-write-seeded build, same compiler profile on both sides.
macOS / APFS.

| Workspace | `cargo check --workspace` | with grove | speedup |
|---|---|---|---|
| ripgrep (61 crates) | 2.6s | 0.7s | 3.7x |
| 570-crate workspace | 39s | 10s | 3.9x |
| 570-crate workspace, once the lane is warm | 39s | 2.8s | 14x |

The seed is a single `clonefile`, so it stays about 2 seconds no matter how large the
workspace is. The cold build grows with the project; the seed does not. The bigger the
codebase, the bigger the win. Reproduce the ripgrep run with
[`benchmark/ripgrep.sh`](benchmark/ripgrep.sh).

## Install

```sh
cargo install --git https://github.com/thomgardiner/grove
```

## Usage

```sh
grove cache warm     # build the shared canonical once, per repo
grove check          # check only what your git diff touched
grove test           # test only the affected packages

grove worktree acquire --agent alice   # fresh worktree on its own branch, prints the path
grove worktree reap                    # reclaim abandoned worktrees, salvaging their work
grove watch                            # daemon: prewarm new worktrees, reap dead ones

grove cache status   # disk and lanes
grove cache gc       # reclaim stale lanes, evict to the disk watermark
```

## Config

Defaults are sensible, so most repos need nothing. To tune it, drop a `.grove.toml` in a
repo or a global one at `~/.config/grove/config.toml`; a per-repo file overrides the
global, and environment variables override both. `grove config` prints the resolved
settings and where the file lives.

```toml
cache_root       = "/fast-disk/grove"  # where lanes and canonicals live
min_free_gb      = 30                   # keep at least this much disk free
max_canonical_gb = 40                   # cap total warm-build cache size (default: unbounded)
worktree_root    = "/work/worktrees"    # where `worktree acquire` puts worktrees
reap_ttl_secs    = 3600                 # idle time before a worktree is abandoned
claim_ttl_secs   = 1800                 # idle time before a work claim expires
keep_debuginfo   = false                # keep debug info in lane builds
require_cow      = false                # refuse to seed if the clone would be a full copy
```

Every key has an environment override: `GROVE_CACHE_ROOT`, `GROVE_MIN_FREE_GB`,
`GROVE_MAX_CANONICAL_GB`, `GROVE_WORKTREE_ROOT`, `GROVE_REAP_TTL_SECS`,
`GROVE_CLAIM_TTL_SECS`, `GROVE_KEEP_DEBUGINFO`, `GROVE_REQUIRE_COW`.

## How it works

- Each worktree gets an isolated, file-locked build directory (a lane) under
  `~/.cargo/grove`, so parallel builds never share a target directory.
- A cold lane is seeded from a warm canonical with one copy-on-write clone (APFS
  clonefile, ReFS block clone, btrfs/XFS reflink). Cargo then rebuilds only what changed.
- The canonical is keyed by the repo's git directory, not `Cargo.lock`, so a dependency
  bump rebuilds a few crates instead of everything.
- `check` and `test` map the git diff to the affected packages with `cargo metadata`.
- Disk is bounded by a free-space watermark and least-recently-used lane eviction.

## License

MIT
