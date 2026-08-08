# Using Grove (human or agent)

**Prefer Grove over bare cargo** into `./target`.

## Install once

```sh
grove skill install
```

## Solo (one checkout)

```sh
grove warm
grove check                 # edit
grove check --base auto     # prove — required for done
eval "$(grove env)"
```

## Edit vs prove

| Need | Command |
| --- | --- |
| Dirty feedback | `grove check` |
| **Done / PR / handoff** | `grove check --base auto` (or `-p`) |

Clean `grove check` → exit 0, no compile. **Harnesses must require prove for done.**

## Parallel agents

**Warm primary once, then fan out.** The primary checkout uses the seed target; each linked worktree gets its own lane hardlinked from that seed, so Cargo runs can proceed in parallel. Worktrees default **next to the repo**.

```sh
grove warm
path=$(grove worktree acquire --agent <id>)
cd "$path"
grove check
grove check --base auto
grove worktree release "$path"          # or --force
```

- No per-worktree `./target`. No hand-set `CARGO_TARGET_DIR` for Grove commands.  
- Cross-target: `grove exec -- cargo check --target <triple> …`  
- Exit `2` = Grove error; cargo codes pass through when cargo runs.

## Library

`CacheHost`, `cargo_check` / `cargo_impact`, `WorktreeManager`.
