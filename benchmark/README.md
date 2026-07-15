# Benchmarking Grove

`head_to_head.mjs` is the reproducible comparison harness. It measures a real fresh
git worktree for each sample, not a manually copied output directory.

```sh
node benchmark/head_to_head.mjs
```

It pins ripgrep `14.1.1`, fetches dependencies before timing, and performs five cyclically
rotated samples of each workload:

- `cargo check --workspace --locked`
- `cargo nextest run --workspace --no-run --locked`

The default matrix is plain Cargo, sccache, cargo-worktree, and Grove. sccache and
cargo-worktree are skipped with a recorded reason when they are not installed. Grove and
sccache are primed outside the measured window: Grove with `grove cache warm`, and sccache
with both workloads in a separate worktree. `CARGO_INCREMENTAL=0` is common to every mode,
because sccache cannot cache incrementally compiled Rust crates.

Each sample runs ripgrep's `rg --version` through the same output directory and must produce
the same SHA-256 as the Cargo baseline. The timed test-binary workload still succeeds only
when `cargo nextest run --no-run` succeeds. The runner writes raw logs, tool versions,
filesystem telemetry, samples, medians, p95s, compile-line counts, and sccache stats to
`benchmark/.work/<run>/report.json`.

The runner uses a private `CARGO_HOME`, `GROVE_CACHE_ROOT`, and sccache directory. It removes
all source checkouts, worktrees, targets, build directories, and compiler caches it created
after writing the report. Set `KEEP_ARTIFACTS=1` only when investigating a failed run.

It refuses to start with fewer than 30 GiB free by default, avoiding a benchmark run that
makes a full development disk worse. Override the safety threshold deliberately with
`BENCH_START_FREE_GB`; select modes with `MODES`; increase repetitions with `RUNS`.
For a different fixture, set `BENCH_PROBE_BIN` and whitespace-separated `BENCH_PROBE_ARGS`.

`ripgrep.sh` remains useful only as a copy-on-write clone microbenchmark. Do not use its
numbers to rank Grove against other tools.

References: [Cargo build cache layout](https://doc.rust-lang.org/cargo/reference/build-cache.html),
[sccache path normalization and Rust caveats](https://github.com/mozilla/sccache), and
[cargo-worktree's worktree-aware target layout](https://docs.rs/crate/cargo-worktree/1.0.0).
