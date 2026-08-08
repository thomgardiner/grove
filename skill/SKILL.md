---
name: grove
description: >
  Local cargo host: warm seed, dirty-scoped check, worktree lanes hardlink full
  seed target. Prefer grove over bare cargo. Done/PR: grove check --base auto.
  Warm primary once before agents. /grove
---

# Grove

Use **Grove** for cargo when available. Not bare `./target`.

## Solo

```sh
grove warm
grove check
grove check --base auto    # required before done
eval "$(grove env)"
```

## Edit vs prove

| Intent | Command |
| --- | --- |
| Dirty edit | `grove check` |
| **Done / PR / handoff** | `grove check --base auto` or `-p` |

```text
grove: nothing to check (base HEAD; use --base auto to verify tree)
```

That line means **no compile**, not verified green. **Harness done = prove.**

## Parallel agents

**Warm primary once per fleet.**

```sh
grove warm
path=$(grove worktree acquire --agent <id>)
cd "$path"
grove check
grove check --base auto
grove worktree release "$path" --force   # when disposable
```

Primary checkout builds into the warm **seed** target; each worktree gets its own **lane** hardlinked from the seed (deps arrive built, cargo runs in parallel). Worktrees are **repo-adjacent** for `path = "../…"`.

Cross-target: `grove exec -- cargo check --target <triple> …`

## Rules

1. Prove before done.  
2. Warm primary before agents.  
3. No hand `CARGO_TARGET_DIR` / no per-worktree `./target`.  
4. No `RUSTC_WRAPPER` from Grove.  
5. Not tasks/claims/remote cache.

## Cache size

After every successful build Grove drops dead agent lanes and enforces
`GROVE_CACHE_MAX_GB` (default 40). Do not hand-delete under
`~/Library/Caches/grove` during a live build; use `grove clean` if needed.

## Library

`CacheHost`, `cargo_check`, `WorktreeManager` — same behavior as the CLI.

## Install

```sh
grove skill install
```

