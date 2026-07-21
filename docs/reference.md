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

## Verification

`grove verify` writes a JSON receipt: argv, checkout state, lane, timing, exit
status, bounded output tails, runner-reported test count. A receipt is command
evidence, not proof of correctness. `task finish` goes verified only when every
required profile has a passing receipt for that exact checkout.

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

## Doctor and events

`doctor` is read-only: linker settings, mold availability, incremental-policy
provenance, and the acceleration watchlist. Fleet events append best-effort
JSONL to `<cache-root>/events/<repo>.jsonl`; gaps are possible, so reconcile
durable state before acting on them.

## Boundaries

Grove owns worktrees, claims, lanes, tasks, receipts. Summoner (or any
orchestrator) owns dispatch, review, revision, reports. In-session agents keep
small coupled edits; independent mutations go out as work orders.
