# Reference

`grove capabilities` reports the machine-readable contracts.

## Build admission

`best_effort` (default): a shared GNU jobserver FIFO on Unix; Cargo governs
itself if setup fails. Each top-level builder holds an implicit slot, so this
reduces contention without hard-capping.

`strict` (Unix): admits at most `max_builders`, reserves one implicit slot
each, exposes `cpu_slots - max_builders` FIFO tokens. Invalid bounds, lock
failures, or unsupported platforms refuse the build. Settings must match while
a pool is live. Lane acquisition waits for a slot; `try_acquire` doesn't. Task
timeouts count admission wait; SIGINT/SIGTERM cancels it.

Descendants inherit the pool, admission, lane, and lifecycle descriptors, so
locks stay live while any descendant runs. A program that closes unknown
descriptors opts out of that protection.

Limits of strict mode: it assumes one top-level jobserver client per admitted
command (true for `check`/`test`, unenforceable for `exec`). It does not limit
memory, I/O, network, jobserver-ignoring processes, or best-effort builds. Set
it globally per cache root if it must cover everything.

## Supervision capabilities

`task exec --capability build` (the default) reserves the task's seeded lane for
the command's lifetime and routes cargo into it. Nested grove builds inside that
command refuse immediately rather than waiting on locks their own supervisor
holds.

`task exec --capability edit` supervises lifetime, signals, deadline, and lease
renewal without reserving a lane or an admission slot. Builds the command runs
acquire lanes themselves, when they build. Supervise agent sessions this way:
under `build`, `max_builders` caps live sessions rather than concurrent
compilers.

The refusal travels in `GROVE_SUPERVISED_LANE`, so it is deadlock avoidance
rather than a control: a child that rebuilds its environment (`env -i`, `sudo`
without `-E`) loses the marker and blocks on its parent's lane again. A
verification command that deliberately invoked a lane-acquiring grove command
now fails fast instead of deadlocking.

## Verification

`grove verify` writes a JSON receipt: argv, checkout state, lane, timing, exit
status, bounded output tails, runner-reported test count. A receipt is command
evidence, not proof of correctness. `task finish` goes verified only when every
required profile has a passing receipt for that exact checkout.

`task begin` pins the digest of the workspace's whole verification policy
(required list plus every profile definition). `task finish` refuses with
`policy_changed` when the policy moved since, so a candidate cannot weaken the
bar it will be judged by. Accept a reviewed change with
`--accept-policy <sha256>` from the refusal. Tasks begun before schema 6 carry
no digest and are evaluated as before.

The digest binds the policy document, not everything a command reads. A profile
that runs `sh ci/verify.sh` can still be weakened by editing that script, and
the same goes for tool binaries on PATH and any file a command opens. Grove
cannot observe those inputs; an orchestrator closes the gap by treating such
files as protected. Like `--allow-unverified`, `--accept-policy` makes drift a
deliberate recorded act rather than an authenticated one: Grove cannot tell an
orchestrator from a candidate at the CLI.

`task status --json` (schema 3): `recorded_verification` (`passed`,
`overridden`, `failed`, `unverified`) and `source_sha256` are durable and
survive worktree release. The `verification` object is a live query and may not
be reproducible later. Pair your own records with task id, terminal status, and
those two fields; Grove doesn't infer orchestration outcomes.

## Portable reuse

`verify query <profile>` reports hit or miss, exit 0 either way. Opt in with
`portable = true`. Only Cargo-native commands qualify (`build`, `check`,
`test`, `bench`, `doc`, `metadata`, `tree`). The controlled environment
fingerprints toolchain inputs and declared `portable_env` values; everything
else is stripped. Plugins (`cargo nextest` included), wrappers, config-injected
env, unstable flags, and custom targets make reuse ineligible instead of
risking a false hit.

A hit requires matching: repo identity, exact HEAD, profile hash and argv,
rustc/Cargo fingerprints, effective Cargo config, controlled environment, and
content-addressed snapshots. Receipts also fingerprint the jobserver variables,
so a different governor can't reuse them.

## Inspection capsules

`inspect acquire` copies the task's full state (committed through untracked)
into a private git repo: no origin, no shared metadata, no hooks, no
credentials. `inspect exec` clears git/SSH credential env, forces empty git
config, makes capsule bytes read-only, caps each hashed log at 1 MiB, and
redigests source and capsule after the command. Drift, timeout, truncation, or
surviving processes: `authorized` is false, exit nonzero.

Windows uses a kill-on-close Job Object; Unix uses a process group (escapable
via `setsid`, so best effort). Neither is a same-user filesystem sandbox.
Platforms with neither mechanism refuse to run inspections.

## Release freeze

Unix only. `release freeze` materializes the captured state into a detached
worktree, runs the named profile in a one-use lane, rechecks both snapshots,
then publishes artifacts with a `manifest.json` of content hashes. Refuses
destinations inside the workspace; claims the output directory atomically.

## Sparse worktrees

Scoped acquisition uses cone-mode sparse checkout only after Grove proves
equivalent Cargo metadata at the selected base; otherwise it falls back to a
full checkout. Requested packages bring their local dependency closure.
Expansion is monotonic; `exec`, verification, task commands, warm, and freeze
convert to full first. This is a size optimization, not a sandbox: git objects
are shared and Grove never auto-shrinks or runs `git clean`.

## MCP server

`grove mcp serve` speaks the Model Context Protocol over stdio, exposing the
coordination surface as tools: `grove_status`, `grove_claim`,
`grove_release_claims`, `grove_task_begin`, `grove_task_status`,
`grove_task_finish`, `grove_worktree_acquire`, `grove_worktree_release`. Any
MCP-client harness registers it like any other server, launched at the
repository root:

```json
{ "mcpServers": { "grove": { "command": "grove", "args": ["mcp", "serve"] } } }
```

Tools-only and poll-based by design: no resources, no subscriptions. Claims and
tasks written over MCP are the same durable records the CLI writes, so sessions
coordinating over the protocol and scripts using the shell see one truth.

Agent identity has no default. Pass `--agent` (or the `agent` tool argument)
with a value unique to the session, or export `GROVE_AGENT` once. Claim
identity is derived from the agent name, and a same-name claim is a renewal,
so two sessions sharing a name take over each other's claims instead of
conflicting.

## Cargo outside Grove

Grove routes builds by setting the lane environment on the commands it spawns.
Plain `cargo` in a Grove worktree therefore builds into a local `target/` and
gets no seeding, no routing, and no admission control. That is a real limit, not
a detail: Makefiles, CI steps, IDE buttons, and rust-analyzer all bypass Grove
unless routed. Run one-off commands through `grove exec -- cargo …`, which takes
a lane per invocation and re-resolves policy each time.

Do not try to export a lane and reuse it. A lane is protected by the lock its
owning process holds; an exported path has no lock, so garbage collection may
evict the directory mid-build, worktree release can race it, the jobserver
descriptors cannot transfer, and the policy is never re-resolved.

For rust-analyzer, give it its own tagged lane rather than the interactive one,
so background checks never contend with foreground or agent builds. Set its
check command to `grove exec --tag rust-analyzer -- cargo check`.

## Why a build rebuilt

`why-rebuilt` runs the routed check in the lane and reports how many units Cargo
reused versus recompiled, then explains each unit Cargo considered stale: an
input changed, an output was missing, RUSTFLAGS differ, a dependency rebuilt.

`why-rebuilt --fresh` answers the same question for a brand-new worktree. It
seeds a throwaway lane from the canonical, measures it, and discards it, so a
healthy cache reports everything reused. A large rebuild count here means
seeding is not delivering, which is otherwise visible only as being slow.

The reused/rebuilt counts come from Cargo's JSON artifacts, not from the dirty
log. Cargo reports a unit dirty only when a stale fingerprint exists, so a lane
that seeded nothing rebuilds everything while reporting no stale units at all;
counting only stale units would call a dead cache healthy.

## Effectiveness output

`check` and `test` print one line to stderr: what was routed, how long it took,
and whether the lane was warm, seeded from the canonical, or the unverified
bootstrap fallback. Grove has no counterfactual for what plain Cargo would have
cost on the same tree, so it reports elapsed time and never claims time saved.
Falling back to the bootstrap lane always says so and why.

`cache status` reports `healthy` and `headroom_bytes` against the enforced
floor, plus `busy_lane_count`. Read those, not the logical sizes: lane and
canonical totals overcount copy-on-write sharing, so many idle lanes with ample
headroom is reclaimable inventory rather than a leak.

## Doctor and events

`doctor` is read-only: linker settings, mold availability, incremental-policy
provenance, and the acceleration watchlist. Fleet events append best-effort
JSONL to `<cache-root>/events/<repo>.jsonl`; gaps are possible, so reconcile
durable state before acting on them.

## Boundaries

Grove owns worktrees, claims, lanes, tasks, receipts. Summoner (or any
orchestrator) owns dispatch, review, revision, reports. In-session agents keep
small coupled edits; independent mutations go out as work orders.
