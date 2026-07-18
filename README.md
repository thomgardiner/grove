# grove

A build cache and worktree manager for Rust. Every git worktree gets its own build
directory, seeded copy-on-write from one shared warm build, so a fresh worktree builds in
seconds instead of recompiling from scratch. grove also runs the worktrees themselves:
handing them out on their own branches, tracking who is working on what so parallel work
does not collide, and reclaiming the ones that get abandoned (salvaging their work first).
One static binary that works with zero setup.

## Benchmarks

The original figures below are copy-on-write seed microbenchmarks, not end-to-end
head-to-head results. They establish that a cloned warm build can be reused; they do not
rank Grove against Cargo, sccache, or cargo-worktree.

| Workspace | cold build | manually seeded output | speedup |
|---|---:|---:|---:|
| ripgrep (61 crates) | 2.6s | 0.7s | 3.7x |
| 570-crate workspace | 39s | 10s | 3.9x |
| 570-crate workspace, once the lane is warm | 39s | 2.8s | 14x |

Run [`benchmark/head_to_head.mjs`](benchmark/head_to_head.mjs) for the reproducible fresh-worktree
comparison. It records median/p95 timing, raw logs, tool versions, filesystem data, and a
binary-output behavior-equivalence gate. [`benchmark/ripgrep.sh`](benchmark/ripgrep.sh) remains the
low-level clone microbenchmark.

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
grove worktree acquire --agent alice --materialize crate:parser  # proved sparse checkout
grove worktree expand PATH crate:cli   # add a package closure; never shrinks
grove worktree full PATH               # convert to a normal full checkout
grove worktree heartbeat PATH          # renew while working outside supervised task exec
grove worktree reap                    # reclaim abandoned worktrees, salvaging their work
grove watch                            # daemon: prewarm new worktrees, reap dead ones

grove cache status   # fast physical disk telemetry and lanes
grove cache status --details  # slow logical per-lane sizes (not physical CoW usage)
grove cache gc       # reclaim stale lanes, evict to the disk watermark
grove doctor          # read-only Rust build-acceleration report; changes no Cargo policy

# Durable task handoff, verification evidence, and recovery.
grove task begin --agent alice --task parser --scope src/parser.rs
grove task exec --task-id TASK_ID -- cargo nextest run -p parser --no-tests fail
grove verify fast --task-id TASK_ID
# Ask whether an exact clean deployment checkout can reuse prior clean evidence.
grove verify query fast
grove task finish --task-id TASK_ID
grove status --watch
grove task reap --dry-run

# Advice for an external orchestrator; Grove never launches agents itself.
grove plan --base main --json

# Export a lane output without discovering Grove's cache layout.
grove artifact export --tag release target/release/my-bin --to ./dist/my-bin --task-id TASK_ID

# Release a claim explicitly, or make one frozen bundle outside the worktree.
grove release claims --agent alice
grove release freeze \
  --task-id TASK_ID --profile release \
  --artifact target/release/my-bin --out ../dist/v0.3.0
```

## Config

Defaults are sensible, so most repos need nothing. To tune it, drop a `.grove.toml` in a
repo or a global one at `~/.config/grove/config.toml`; a per-repo file overrides the
global, and environment variables override both. `grove config` prints the resolved
settings and where the file lives.

```toml
cache_root       = "/fast-disk/grove"  # where lanes and canonicals live
min_free_gb      = 30                   # explicit reserve; default is 5% clamped to 20–50 GiB
max_canonical_gb = 40                   # cap total warm-build cache size (default: unbounded)
worktree_root    = "/work/worktrees"    # where `worktree acquire` puts worktrees
reap_ttl_secs    = 3600                 # idle time before a worktree is abandoned
claim_ttl_secs   = 1800                 # idle time before a work claim expires
cpu_slots        = 8                    # shared build token pool across every lane (default: core count)
keep_debuginfo   = false                # keep debug info in lane builds
require_cow      = false                # refuse to seed if the clone would be a full copy

[worktree]
# Always include generated schemas/config needed by materialized package scopes.
materialize = ["schemas/generated"]

[verification]
required = ["fast"]                    # task finish checks these against its current checkout

[verification.profiles.fast]
continue_on_failure = false
# This remains local evidence. Set `portable = true` only for the built-in Cargo
# commands described below; external tools such as `cargo nextest` deliberately rerun.
portable_env = ["NEXUS_RELEASE_MODE"]
commands = [
  { argv = ["cargo", "nextest", "run", "--workspace", "--locked", "--no-tests", "fail"], allow_zero_tests = false },
]

# Profiles stay serial by default. Give commands IDs and explicit resources to run
# independent gates concurrently; `needs` preserves required ordering.
[verification.profiles.ci]
continue_on_failure = true
max_parallel = 3
cpu_slots = 4
memory_mib = 8192
commands = [
  { id = "lint", argv = ["cargo", "clippy", "--workspace", "--", "-D", "warnings"], allow_zero_tests = false, cpu = 1, memory_mib = 1024 },
  { id = "test", argv = ["cargo", "nextest", "run", "--workspace", "--no-tests", "fail"], allow_zero_tests = false, cpu = 3, memory_mib = 4096 },
  { id = "docs", needs = ["lint"], argv = ["cargo", "test", "--doc", "--workspace"], allow_zero_tests = false, cpu = 1, memory_mib = 1024 },
]
```

Every key has an environment override: `GROVE_CACHE_ROOT`, `GROVE_MIN_FREE_GB`,
`GROVE_MAX_CANONICAL_GB`, `GROVE_WORKTREE_ROOT`, `GROVE_REAP_TTL_SECS`,
`GROVE_CLAIM_TTL_SECS`, `GROVE_KEEP_DEBUGINFO`, `GROVE_REQUIRE_COW`.

`grove verify` stores JSON receipts under Grove's cache with the exact argv, checkout state,
lane, timing, exit status, bounded output tails, and any runner-reported test count. They are
command evidence, not a claim that an artifact or behavior is correct. `task finish` only marks a
task verified when every configured required profile has a successful receipt for that exact
checkout state.

`grove verify query <profile>` emits JSON and exits zero for both hits and misses. A profile opts
in with `portable = true`; Grove then accepts only Cargo-native `build`, `check`, `test`, `bench`,
`doc`, `metadata`, `tree`, and version/help commands. It runs those commands
with a controlled environment: standard Rust/Cargo/toolchain inputs and declared `portable_env`
values are fingerprinted, while undeclared shell/session variables are not passed through. Cargo
plugins (including `cargo nextest`), `cargo rustc`/`cargo rustdoc`, custom compiler/linker
wrappers, Cargo path overrides/config-injected environment, CLI `--config`, unstable flags,
alternate lockfiles/custom target JSON, and ignored workspace files make reuse ineligible rather
than risking a false hit.

A hit may come from another clean clone sharing Grove's cache only when the origin repository
identity, exact HEAD, profile hash and argv, rustc/Cargo fingerprints, effective Cargo config,
controlled build environment, and content-addressed input/output snapshots all match.
Dirty checkouts, old receipts without the portable binding, different commits, commands, remotes,
or toolchains are misses so a deployment script can run its normal gate.

`grove doctor` is read-only. It reports repository-local linker settings and Linux mold
availability without changing them, flags optimized Cargo profiles that disable incremental
compilation (including the setting's provenance), and records the current watchlist for nightly
parallel rustc front-end work, Cargo relink-don't-rebuild, Wild, and sccache. Incremental policy
is part of Grove's lane identity and receipt lane identity; repositories choose whether to enable
it after measuring their own runtime and disk trade-off.

On Unix, `grove release freeze` materializes the captured tracked, dirty, untracked, and deleted content
into a detached worktree, runs the named serial profile there in a fresh one-use lane, and rechecks
both snapshots before publishing. Requested artifacts must therefore be created by that invocation;
Grove preserves their modes and emits a `manifest.json` recording their content hashes. It refuses
destinations inside the workspace and atomically claims an output directory rather than replacing
an existing one. Other platforms currently fail closed before staging or executing release commands.

## Automation and agents

`grove init` drops the agent contract into a repository: an `AGENTS.md` section any
harness reads (Codex, Claude, OpenCode, anything driving a shell) plus a commented
`.grove.toml` starter. Exit codes are stable: 0 is success, 1 is a domain refusal (claim
conflict, failed verification, failing tests), anything else is an error. Most commands
print JSON. Agents outside supervised `grove task exec` commands run
`grove worktree heartbeat PATH` periodically; nonterminal tasks and live lanes also
protect managed worktrees from reaping.

Scoped worktrees use Git cone-mode sparse checkout only after Grove proves equivalent
Cargo metadata at the exact selected base. Requested packages include their local
dependency closure; other workspace packages retain the manifest/target skeleton Cargo
needs. Unsupported layouts, root packages, failed proof, or negligible savings fall back
to a normal full checkout. For a clean current base, Grove plans before
`git worktree add --no-checkout`, so excluded files are never populated; historical or
dirty-source bases use a full safety bootstrap. Expansion is monotonic. Affected
`check`/`test` expands its package closure, while opaque consumers (`exec`, verification,
task commands, cache warm, and release freeze) convert full before starting a lane or
child.

This is a checkout-size optimization, not a sandbox or a claim boundary. Git objects are
still shared, reported byte counts are logical rather than physical CoW allocation, and
an absent sparse path is not a deletion or task write. Grove never automatically shrinks
an active checkout, runs `git clean`, or reapplies sparsity during cleanup.

Fleet events attempt to append to `<cache-root>/events/<repo>.jsonl` (claims, tasks,
verifications, reaps). JSONL is a low-latency best-effort signal, not a durable replay
log: rotation or write failure can create gaps. Consumers reconcile durable task, claim,
lease, and receipt state before acting.

## Routing and authority

- Read-only exploration stays with the current session's in-process agents; make small,
  coupled edits directly.
- Dispatch independent mutations as Summoner work orders. Plan campaigns, run them as a
  fleet, independently review and revise them, then reconcile them against durable evidence.
- The session harness owns in-session fan-out; Summoner owns fleet `max_parallel` (default
  2); Grove owns build `cpu_slots` (default: core count).

Grove owns worktrees, claims, lanes, tasks, and verification receipts. Summoner owns executor
dispatch, independent review, revision, and run reports.

## How it works

- Each worktree gets an isolated, file-locked build directory (a lane) under
  `~/.cargo/grove`, so parallel builds never share a target directory.
- A cold lane is seeded from a warm canonical with one copy-on-write clone (APFS
  clonefile, ReFS block clone, btrfs/XFS reflink). Cargo then rebuilds only what changed.
- The canonical is keyed by the repo's git directory, not `Cargo.lock`, so a dependency
  bump rebuilds a few crates instead of everything.
- `check` and `test` map the git diff to the affected packages with `cargo metadata`.
- Materialized worktrees omit unrelated working files while retaining the complete Cargo
  graph; sparse-aware snapshots read absent tracked content from Git index objects.
- Disk is bounded by a free-space watermark and least-recently-used lane eviction. Every
  Grove-managed compiler command runs this maintenance before and after it owns a lane.
- Grove owns its lanes and canonicals, not `target/` directories made by direct Cargo, IDE,
  or script invocations. Route ad-hoc Cargo through `grove exec --tag <name> -- cargo ...`
  to keep new artifacts inside the managed budget.

## License

MIT
