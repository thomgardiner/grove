# Seeded-lane benchmark, 2026-07-21

Fixture: ripgrep 14.1.1 (fresh `--depth 1` clone). Machine: Apple M2 Max, 12 cores,
32 GiB, macOS 25.5.0, APFS on an external volume. Tools: cargo 1.97.1, rustc 1.97.1,
nextest 0.9.140, grove 0.3.4 built from the tree published as commits `4f84321` and
`b5ca038`. Harness: `benchmark/head_to_head.mjs`, `PHASES=cold,seeded RUNS=4
CONCURRENCY=1 MODES=cargo-default,grove`, per-sample mode rotation.

Every sample's `rg` behavior probe matched the cargo-default SHA-256 baseline
(`matches_cargo_default: true` throughout `report.json`).

| phase | workload | mode | median | p95 | compile units |
|---|---|---|---|---|---|
| seeded | check | cargo-default | 3.45s | 3.64s | 40 |
| seeded | check | grove | **1.32s** | 1.46s | **10** |
| seeded | test-binaries | cargo-default | 11.74s | 12.04s | 48 |
| seeded | test-binaries | grove | **8.54s** | 8.96s | **10** |
| cold | check | cargo-default | 3.20s | 6.85s | 40 |
| cold | check | grove | 5.53s | 6.80s | 40 |
| cold | test-binaries | cargo-default | 12.38s | 14.79s | 48 |
| cold | test-binaries | grove | 11.82s | 19.05s | 48 |

Reading: in the seeded phase (a fresh worktree while a warm canonical exists —
Grove's product scenario) Grove reuses all 30 external-dependency units; the 10
that rebuild are ripgrep's own workspace crates, which Cargo re-fingerprints in
any new checkout. The cold phase is the honest tax: a first-ever build pays the
serialized bootstrap-lane cost.

Limitations of this run: single machine, single fixture, `CONCURRENCY=1` only
(no concurrent-group makespan), no RSS or cache hit-rate telemetry, and the
warm/amortized phases were not sampled. Raw per-sample data, storage telemetry,
and full tool/filesystem details are in `report.json`.
