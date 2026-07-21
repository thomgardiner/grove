# grove

Verified local execution for parallel Rust development. Grove routes each change
to the affected Cargo packages, gives every worktree an isolated build lane
seeded copy-on-write from a shared warm build, and keeps concurrent builders
from stampeding the same build state. Run it underneath Codex, Claude Code, or
any other local coding agent.

## Install

macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/thomgardiner/grove/releases/latest/download/grove-installer.sh | sh
```

Windows PowerShell:

```powershell
$ErrorActionPreference = "Stop"
irm https://github.com/thomgardiner/grove/releases/latest/download/grove-installer.ps1 | iex
```

The installer verifies the release checksum and also installs `grove-update`.
Source install: `cargo install --git https://github.com/thomgardiner/grove --locked`.
See the [quickstart](docs/quickstart.md) for upgrades and removal.

## Use

```sh
grove init           # drop the agent contract + a commented .grove.toml starter
grove cache warm     # build the shared canonical once, per repo
grove check          # check only what your git diff touched
grove test           # test only the affected packages
grove exec --tag ci -- cargo bench   # anything else, in an isolated lane
```

Worktrees and coordination:

```sh
grove worktree acquire --agent alice   # fresh worktree on its own branch
grove claim --agent alice --task "parser fix" crate:parser
grove status --watch
```

Durable tasks, verification, and review:

```sh
grove task begin --agent alice --task parser --scope src/parser.rs
grove task exec --task-id ID -- cargo nextest run -p parser
grove verify fast --task-id ID
grove task finish --task-id ID
grove inspect acquire --task-id ID     # digest-bound review capsule
```

Exit codes are stable: 0 success, 1 domain refusal (claim conflict, failed
verification), anything else an error. Most commands print JSON.

## Config

Defaults are sensible; most repos need nothing. Tune with `.grove.toml` in the
repo or `~/.config/grove/config.toml` (repo overrides global, `GROVE_*`
environment variables override both, `grove config` prints the result):

```toml
cache_root       = "/fast-disk/grove"
min_free_gb      = 30
governor_mode    = "best_effort"   # or "strict" for fail-closed admission

[verification]
required = ["fast"]

[verification.profiles.fast]
commands = [
  { argv = ["cargo", "nextest", "run", "--workspace", "--locked", "--no-tests", "fail"], allow_zero_tests = false },
]
```

The full key list, verification profile options, strict admission semantics,
portable receipt rules, and the inspection contract live in
[docs/reference.md](docs/reference.md).

## How it works

- Each worktree gets an isolated, file-locked build lane, so parallel builds
  never share a target directory.
- A cold lane is seeded from the warm canonical with one copy-on-write clone
  (APFS clonefile, ReFS block clone, btrfs/XFS reflink); Cargo rebuilds only
  what changed.
- The canonical is keyed by the repo's git directory, not `Cargo.lock`, so a
  dependency bump rebuilds a few crates instead of everything.
- `check` and `test` map the git diff to affected packages with `cargo metadata`.
- Disk stays bounded: lifecycle cleanup, a free-space watermark, and LRU lane
  eviction run around every managed compiler command.
- Verification receipts record command evidence against the exact checkout;
  `task finish` goes green only when the required profiles have receipts for
  that state.

## Benchmarks

`benchmark/head_to_head.mjs` is the reproducible comparison: fresh worktrees,
median/p95, compile-unit counts, and a binary-output behavior-equivalence gate.
Published runs live in [benchmark/reports/](benchmark/reports/).

## License

MIT
