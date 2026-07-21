# grove

Verified local execution for parallel Rust development. Grove routes each change to the
affected Cargo packages, gives every worktree an isolated build lane seeded copy-on-write
from a shared warm build, and coordinates concurrent builders so agents do not corrupt or
stampede the same build state. Claims keep parallel edits disjoint, while repository-owned
verification records evidence against the exact candidate before handoff or release.

Use Grove underneath Codex, Claude Code, or any other local coding agent. It manages the
Rust-specific build and verification layer that generic worktrees leave to each agent:
affected-package checks, warm isolated lanes, build-resource admission, durable task state,
and digest-bound inspection snapshots.

## Benchmarks

Run [`benchmark/head_to_head.mjs`](benchmark/head_to_head.mjs) for the reproducible fresh-worktree
comparison. It records median/p95 timing, raw logs, tool versions, filesystem data, and a
binary-output behavior-equivalence gate. [`benchmark/ripgrep.sh`](benchmark/ripgrep.sh) remains the
low-level clone microbenchmark. Treat performance as environment-specific: publish dated results
only with the generated report, hardware and filesystem details, tool versions, and passing
behavior-equivalence evidence.

## Install

From source (requires a Rust toolchain):

```sh
cargo install --git https://github.com/thomgardiner/grove --locked
```

Prebuilt binary installers ship with the first published GitHub release; until then,
source installation is the supported path.

Then, from a Rust repository:

```sh
grove init
grove doctor
grove cache warm
grove check
grove test
```

See the [five-minute quickstart](docs/quickstart.md) for upgrades and safe removal.

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

grove cache status   # physical free-space reserve, canonical/lane inventory, logical-budget setting
grove cache status --details  # slow logical file sizes; CoW sharing means they are not physical disk use
grove cache gc       # reclaim stale lanes; enforce physical free-space watermark and logical canonical retention budget
grove doctor          # read-only Rust build-acceleration report; changes no Cargo policy

# Durable task handoff, verification evidence, and recovery.
grove task begin --agent alice --task parser --scope src/parser.rs
grove task exec --task-id TASK_ID -- cargo nextest run -p parser --no-tests fail
grove verify fast --task-id TASK_ID
# Ask whether an exact clean deployment checkout can reuse prior clean evidence.
grove verify query fast
grove task finish --task-id TASK_ID
grove task status TASK_ID --json
grove status --watch
grove task reap --dry-run

# Digest-bound inspection; exec caps each hashed output log at 1 MiB.
grove inspect acquire --task-id TASK_ID
grove inspect exec CAPSULE_ID --timeout-secs 900 -- reviewer --json
grove inspect release CAPSULE_ID
grove inspect reap --dry-run
grove capabilities

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
governor_mode    = "best_effort"        # best_effort (default) or strict
cpu_slots        = 8                    # direct single-client build budget (default: core count)
max_builders     = 2                    # admitted builders in strict mode (default: 1)
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
`GROVE_CLAIM_TTL_SECS`, `GROVE_GOVERNOR_MODE`, `GROVE_CPU_SLOTS`,
`GROVE_MAX_BUILDERS`, `GROVE_KEEP_DEBUGINFO`, `GROVE_REQUIRE_COW`.

The default `best_effort` governor shares a GNU jobserver FIFO on Unix and lets Cargo
govern itself if setup fails. Because each top-level builder owns an implicit slot, it
reduces contention but is not a hard cap. Unix users who need fail-closed admission can
set `governor_mode = "strict"`: Grove then admits at most `max_builders`, reserves one
implicit slot for each, and exposes only `cpu_slots - max_builders` FIFO tokens. Strict
settings must match while a strict pool is live; invalid bounds, FIFO/lock failures, and
unsupported platforms refuse the build. Malformed or zero strict limits also refuse the
build instead of falling back to defaults.

When every strict builder slot is occupied, normal lane acquisition waits, while
`try_acquire` does not wait for a lane or strict builder slot. Task timeouts count time
spent waiting for strict builder admission; SIGINT or SIGTERM also cancels that wait
before an executor is spawned.

On Unix, strict commands receive the same jobserver through `CARGO_MAKEFLAGS`, `MAKEFLAGS`,
and `MFLAGS`. Grove also passes the pool membership, builder admission, lane, and workspace
lifecycle descriptors to descendants. Those locks therefore remain live if the direct
Grove child exits while its descendants still run. A program that deliberately closes
unknown descriptors opts out of that descendant protection; descriptor inheritance is the
OS enforcement boundary.

Strict mode is a fail-closed admission policy, not a universal CPU cap. Its `cpu_slots`
accounting assumes each admitted command starts at most one top-level GNU jobserver
client; the direct Cargo commands used by `grove check` and `grove test` follow that rule.
`grove exec` accepts arbitrary commands and cannot enforce it: a shell that launches multiple
top-level Cargo clients can consume an implicit slot per client and exceed `cpu_slots`.
Strict mode also does not limit memory, disk I/O, network traffic, processes that ignore
the jobserver, best-effort builds, or unrelated same-user processes. Configure it globally
for every Grove process sharing a cache root when the admission policy must cover all Grove
builds. Portable verification receipts fingerprint the exact jobserver variables used by
their controlled child environment, so a mismatched governor cannot reuse the receipt.

`grove verify` stores JSON receipts under Grove's cache with the exact argv, checkout state,
lane, timing, exit status, bounded output tails, and any runner-reported test count. They are
command evidence, not a claim that an artifact or behavior is correct. `task finish` only marks a
task verified when every configured required profile has a successful receipt for that exact
checkout state.

`grove task status --json` schema 3 includes `recorded_verification`, the durable state written by
`task finish` (`passed`, `overridden`, `failed`, or `unverified`), and `source_sha256`, the nullable
inspection source digest bound by the first terminal finish. These fields remain authoritative after
a managed worktree is released. The separate `verification` object is a live freshness query against
the task workspace and may no longer be reproducible after that workspace is removed. External
controllers should pair their own recorded receipt details with the matching task id, terminal status,
`recorded_verification`, and `source_sha256`; Grove does not infer orchestration outcomes.

`grove inspect acquire` captures the task's committed, staged, unstaged, deleted, and untracked
state into a private Git repository with no shared common directory, origin, alternates, refs,
hooks, credential configuration, or index. `inspect exec` rechecks the source and capsule before
launch, clears common Git/SSH credential environment channels and forces empty Git configuration,
makes capsule bytes read-only using the host's portable permission API, captures stdout/stderr to
hashed log files capped at 1 MiB each, waits for supervised process cleanup, and rechecks both
digests. A truncated log is reported explicitly and cannot authorize the result. Any command
failure, timeout, source drift, capsule mutation, output truncation, or surviving process tree makes
`authorized` false and exits nonzero. Platforms without Unix process groups or Windows Job Objects
refuse inspection execution instead of running without containment.

On Windows the reviewer runs in a kill-on-close Job Object. A blocked Grove helper is assigned
before it may spawn the reviewer, closing the spawn-to-assignment escape window. On Unix Grove uses
a dedicated process group and kills remaining members before redigesting. A process that calls
`setsid` can escape that group, so the Unix guarantee is explicitly best effort; this is not a
universal same-user filesystem sandbox. `grove capabilities` reports the exact status, task-record,
inspection, filesystem, and process-tree contracts for machine clients.

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
  2); Grove applies the configured best-effort or strict GNU jobserver policy.

Grove owns worktrees, claims, lanes, tasks, and verification receipts. Summoner owns executor
dispatch, independent review, revision, and run reports.

## How it works

- Each worktree gets an isolated, file-locked build directory (a lane) under
  `~/.cargo/grove`, so parallel builds never share a target directory.
- Before an authoritative canonical exists, ordinary builds for one workspace and
  policy share a serialized bootstrap lane. Different worktrees never share unverified
  artifacts. A failed or interrupted command clears its success marker before mutation,
  so partial output never becomes retention evidence.
- A cold lane is seeded from a warm canonical with one copy-on-write clone (APFS
  clonefile, ReFS block clone, btrfs/XFS reflink). Cargo then rebuilds only what changed.
- The canonical is keyed by the repo's git directory, not `Cargo.lock`, so a dependency
  bump rebuilds a few crates instead of everything.
- `check` and `test` map the git diff to the affected packages with `cargo metadata`.
- Materialized worktrees omit unrelated working files while retaining the complete Cargo
  graph; sparse-aware snapshots read absent tracked content from Git index objects.
- Disk is bounded by lifecycle cleanup, a free-space watermark, and least-recently-used
  lane eviction. Successful bootstraps replace redundant cold lanes for their workspace, and an
  atomically published canonical replaces its bootstrap. Every Grove-managed compiler
  command runs maintenance before and after it owns a lane.
- Grove owns its lanes and canonicals, not `target/` directories made by direct Cargo, IDE,
  or script invocations. Route ad-hoc Cargo through `grove exec --tag <name> -- cargo ...`
  to keep new artifacts inside the managed budget.

## License

MIT
