# grove

**Agentic Rust build tooling.** Many AI agents, each in its own git worktree, all
building the same Cargo workspace — without waiting on cold compiles, without a
`target/` folder per worktree ballooning your disk, without symlink hacks.

grove gives every worktree an isolated build **lane**, seeds a fresh lane
**copy-on-write** from one warm **canonical** (so a new worktree reuses the whole
compiled dependency graph instead of cold-building it), routes `check`/`test` to only
the packages your **git diff** touches, and keeps the shared cache **self-bounding** on
disk. One static binary, drops into any Cargo workspace.

```sh
cargo install grove          # or build this repo and symlink target/release/grove

cd any-rust-project
grove cache warm             # once per repo: build the canonical every worktree seeds from
grove check                  # smart-route: check only what your diff changed
grove test                   # smart-route: test only the affected packages
grove check -p my-crate      # or target a package explicitly
grove cache status           # disk + lanes
grove cache gc               # reclaim orphaned lanes + evict to the disk watermark
```

## Why it's built for agents

- **Isolated lanes, shared root.** Each `(workspace, toolchain)` gets its own
  build/target dir under `~/.cargo/grove`, locked, so N agents never fight one mutable
  `target/` or its build lock. A fresh worktree's lane is a copy-on-write clone of the
  canonical — separate inodes sharing disk blocks, so a lane build can never corrupt the
  root, and seeding is near-free (APFS clonefile, ReFS block clone, Linux reflink).
- **Route by diff.** `grove check`/`test` with no `-p` map the git diff to workspace
  packages via `cargo metadata` and build the reverse-dependency closure. A leaf edit
  checks a few crates, a shared-crate edit fans out, a `Cargo.lock`/toolchain change goes
  workspace-wide, a docs-only change is a no-op.
- **Agent-optimized builds.** Agents never need backtraces or dSYM, so grove lanes build
  with `debug = 0` and no split-debuginfo: a large, safe incremental win that would be
  wrong to force on a human's lane.
- **Self-bounding disk.** The cache is bounded by a free-disk **watermark** on the real
  volume (the only copy-on-write-safe signal — logical file sizes overcount shared
  blocks), plus **stale-lane GC** (a lane whose worktree is gone is pure garbage) and
  whole-lane LRU. It replaces space as you go instead of only ever growing.

## Model

- **Lane**: one `(workspace, toolchain)`'s isolated build dir, keyed by path hash, held
  under an exclusive lock while in use.
- **Canonical**: one warm full-workspace build per `(repo, toolchain)`, keyed by the
  shared `.git` dir so every worktree of a repo seeds from it. Deliberately **not** keyed
  on `Cargo.lock`: a dep bump would otherwise force a cold rebuild, whereas seeding from a
  drifted canonical rebuilds only the changed deps.
- **Seed**: `grove` clones the canonical into a cold lane copy-on-write, then builds only
  what changed on top of it. `cache warm` builds the canonical (both check-mode and
  test-mode, so scoped lanes reuse it) and promotes it.

## Getting the most out of a seed

A seed only helps a build unit whose `(mode, target-set, feature-set)` matches what the
lane asks for. `cache warm` covers check `--all-targets` and `nextest --no-run` for the
first two. For the third (features), under resolver v2 a scoped `-p` build can resolve a
shared dep to a different feature set than `--workspace`; fix it per project with
[`cargo hakari`](https://docs.rs/cargo-hakari). And run `git-restore-mtime` on worktree
create so a fresh checkout's newer source mtimes don't spuriously rebuild unchanged
crates.

## License

MIT OR Apache-2.0.
