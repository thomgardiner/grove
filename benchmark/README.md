# Benchmarking Grove

`head_to_head.mjs` is the repeatable evidence harness for Grove's narrow claim:
safe, warm, isolated build lanes for concurrent Rust worktrees. It does not claim that
Grove beats a single already-warm Cargo checkout.

```sh
node benchmark/head_to_head.mjs
```

Its default matrix is six modes, four phases, both serial and concurrent runs, and six
samples per cell (`RUNS=6`, `CONCURRENCY=2`):

- `cargo-default`: Cargo's ordinary per-worktree `target` directory.
- `cargo-isolated`: Cargo with an explicit private target and build directory per worktree.
- `cargo-shared`: one explicit target/build directory shared by all worktrees. Cargo's own
  locking serializes access; this is a legitimate baseline, not an unsafe configuration.
- `cargo-worktree`: cargo-worktree when installed.
- `grove`: Grove lanes, warmed with `grove cache warm` when a phase needs warm state.
- `sccache`: Cargo plus sccache when installed. Its absence is recorded in the report.

Phases are `cold` (no priming), `seeded` (Grove's canonical is warmed once with
`grove cache warm` on the source checkout, then each sample times the workload in a brand-new
worktree — Grove seeds its lane copy-on-write from the canonical while `cargo-default` starts
from an empty target; only those two modes define seeding), `warm` (a first command
establishes warm state in each fresh isolated worktree, then `seconds` records a repeated
command in that same retained worktree), and `amortized` (that first-command cost is included
in `amortized_seconds`).
`fresh_seconds` records the first command separately, so the report never compares Cargo
worktrees prebuilt individually with a Grove worktree paying its first lane seed. The
fresh command includes each mode's actual lane/materialization path; it is not a claim
that Cargo has a canonical-lane cache. Every
cell runs `cargo check` and Nextest test-binary compilation. A deterministic `rg` behavior
probe produces a SHA-256 receipt; every result must match `cargo-default` for that workload.
Command exit outcomes and raw logs are preserved too.

The report at `benchmark/.work/<run>/report.json` contains samples, medians/p95s, compile
line counts, logical storage and physical allocation where `stat.blocks` is available,
machine/OS/filesystem/toolchain details, enabled capabilities, and Grove's cache-status JSON
when available. `GROVE_REQUIRE_COW=true` is recorded as a request, not misreported as a
filesystem guarantee; the cache-status evidence is retained for interpretation.

Use `--dry-run` to inspect the chosen matrix without cloning or building. Select a smaller
run deliberately, for example:

```sh
RUNS=2 CONCURRENCY=4 PHASES=cold,warm MODES=cargo-default,cargo-shared,grove \
  node benchmark/head_to_head.mjs
```

The runner uses private Cargo, Grove, sccache, target, build, source, and worktree paths
under its run root. It refuses to start below 30 GiB free by default and cleans those owned
artifacts after writing the report. Set `BENCH_START_FREE_GB` only deliberately and
`KEEP_ARTIFACTS=1` only to investigate a failure. `BENCH_REPO`, `BENCH_REF`,
`BENCH_PROBE_BIN`, and whitespace-separated `BENCH_PROBE_ARGS` choose another fixture.

For a second, reproducible workload that does not vendor another project, use the generated
24-crate dependency-chain workspace. It is deliberately dependency-free and is reported as a
synthetic fixture, so it complements rather than substitutes for real-project measurements:

```sh
BENCH_FIXTURE=medium RUNS=3 CONCURRENCY=4 PHASES=cold,warm \
  MODES=cargo-default,cargo-isolated,cargo-shared,grove \
  node benchmark/head_to_head.mjs
```

The fixture generator is versioned in `benchmark/fixture.mjs`; the report records
`synthetic:medium` and its fixture version. Its binary prints a deterministic value used for the
same behavior-receipt check as the ripgrep workload.

`ripgrep.sh` is solely a copy-on-write clone microbenchmark; it is not comparative evidence.

References: [Cargo build cache layout](https://doc.rust-lang.org/cargo/reference/build-cache.html),
[Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html), and
[sccache](https://github.com/mozilla/sccache).
