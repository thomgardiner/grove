# Agent guide

<!-- grove:agents:v1 -->
## Grove: build and coordination contract

Every agent in this repository coordinates through `grove`'s registry; in a
Cargo workspace it also builds through grove. These rules keep many parallel
agents from corrupting builds or each other's work. Most commands print JSON.
Exit codes: 0 is success, 1 is a domain refusal (claim conflict, failed
verification, failing tests), anything else is an error.

Build and test (never run plain cargo in a shared checkout):

- `grove check` / `grove test` route to the packages affected by your git diff.
- `grove exec --tag <gate> -- <command>` runs anything else in an isolated lane.
- Never set `CARGO_TARGET_DIR`, `CARGO_BUILD_BUILD_DIR`, or `MAKEFLAGS`; grove owns
  lane isolation and the machine-wide build governor.

Coordinate before writing:

- `grove status --json` shows every live claim and task; check it first.
- `grove claim --agent <stable-id> --task "<what>" <paths|crate:name ...>` claims
  your scope. First wins; a conflict exits 1. Claims expire after the claim TTL
  (default 30 minutes); re-running the same claim renews it.
- Work longer than a few minutes belongs in a durable task instead:
  `grove task begin --agent <id> --task "<what>" --scope <path ...>`, then
  `grove task exec --task-id <id> -- <command>`. Command heartbeats keep it alive.
- `grove verify <profile> --task-id <id>` records verification receipts.
- `grove task finish --task-id <id>` needs fresh receipts for the repository's
  required profiles, or an explicit `--allow-unverified "<reason>"`, which is
  recorded. Writes outside the task's declared scope block finish unconditionally.
- Release standalone claims with `grove release claims --agent <id>`.

Worktrees:

- `grove worktree acquire --agent <stable-id>` gives an isolated checkout on its
  own branch; `grove worktree release <path>` only after the work is landed.
- For large repositories, request a proved sparse checkout with
  `grove worktree acquire --agent <id> --materialize crate:<name>`. Add scope with
  `grove worktree expand PATH <scope...>` or convert permanently with
  `grove worktree full PATH`; expansion never shrinks an active checkout.
- Sparse checkout is a size optimization, not a sandbox or claim. Affected builds
  expand package closure; opaque commands, verification, task exec, cache warm, and
  release freeze convert full before launching.
- Agents outside supervised `grove task exec` commands run
  `grove worktree heartbeat PATH` periodically while they own the checkout.
- Idle worktrees are reaped after the TTL; committed and dirty work is salvaged to
  the worktree's branch first; nonterminal tasks and live lanes also protect work.

Observe the fleet with `grove status --json --watch` and the event signal at
`<cache-root>/events/<repo>.jsonl` (claims, tasks, verifications, reaps).
JSONL is a low-latency best-effort signal: rotation or write failure can create gaps.
Consumers reconcile durable task, claim, lease, and receipt state before acting.

Keep `docs/ai/` to exactly `RECURRING_BUGS.md`, `DEBUG_RECIPES.md`, and
`LESSONS_LEARNED.md`, recording continuity notes in Symptom/Cause/Fix form.

<!-- summoner:agents:v1 -->
## Summoner: fleet orchestration contract

You (the session reading this) are the orchestrator. Summoner runs fleets of
executor agents inside grove-managed worktrees, and it owns the whole grove
lifecycle for delegated work, so prefer it over hand-driving
`grove task begin/exec/finish`. Inline changes are different. For those,
`grove check` / `grove test` remain yours.

1. Decompose the plan into work orders: one TOML or JSON file per independent
   task in an `orders/` directory. Decompose along the real package seams
   (`grove plan --topology` prints them, with the claim scope owning each);
   keep scopes tight and give every order explicit acceptance criteria and a
   verify profile. Then `summoner plan orders/` refutes the batch before any
   worktree is spent: claim conflicts, package couplings, suggested waves,
   and missing `after` edges. Revise until `clean`.
2. Preflight with `summoner doctor`: it checks each configured executor binary
   and its required environment, and the grove version.
3. `summoner run orders/` executes the fleet. Each order gets an isolated
   worktree, a grove task holding its scope claim, the configured executor CLI,
   then verification. Exit 0: every order verified. Exit 1: at least one order
   needs review. Exit 2: usage or infrastructure error. Add `--stream` for
   NDJSON lifecycle events on stdout (final line: a `report` event with the
   full report); every run also writes the same events to `events.jsonl` in
   the run directory for live monitoring.
4. Read the ranked JSON report (stdout, and report.json in the run directory).
   Review worst-first. Diffs live on each order's branch; verification receipts
   and log tails are in the report. Re-dispatch failures with revised orders,
   or `summoner resume <run-id>` to re-run only what did not succeed. Set
   `fail_fast = N` in `.summoner.toml` so a doomed fleet stops early. Never
   accept work from an executor's claim alone; the receipts are the evidence.

Work order fields: `id`, `title`, `brief`, `scope` (paths or `crate:<name>`),
`acceptance` (list), `verify_profile`, `executor`, `timeout_secs`, `after`.
Chain dependent work with `after = ["<id>"]`: one run executes the whole DAG,
and dependents of failed orders come back `skipped`. The chain is ordering
only, so an order that builds on a dependency's changes must also set
`base = "grove/smn-<dep-id>"` (branch names are deterministic). Executors are
argv templates defined by the user, personal ones in
`~/.config/summoner/config.toml` (template via `summoner init --global`) and
repo overrides in `.summoner.toml`; `summoner config` prints the resolved
settings and their sources, and `summoner doctor` says what is missing.
