# Charter

Grove owns the **local cargo host** for a Rust repo: where builds go, which packages run, how worktree lanes reuse a warm seed.

## Model

| Piece | Meaning |
| --- | --- |
| **Host** | git common-dir + rustc under `$GROVE_CACHE` |
| **Scope** | Git plan → `cargo -p` (edit: dirty; prove: `--base auto`) |
| **Reuse** | Default **shared seed target** + locked **stable-src** view (same abs path for path PackageIds). `GROVE_ISOLATED_LANES=1` for per-worktree targets + hardlink |
| **Worktrees** | Default **repo-adjacent** (`<parent>/<repo>-worktrees`) so `path = "../sibling"` still resolves |
| **Hygiene** | Idle lanes; seed only with `--seed --force`; orphans only under cache or default adjacent parent |

## Edit vs prove

- **Edit:** dirty typecheck; clean tree is a successful no-op.  
- **Prove:** `--base auto` or `-p` — required for done / PR / agent handoff.  
- **Fleet:** warm primary once, then acquire lanes.

## Platforms

macOS / Linux dogfood. Windows: same CLI; hardlink same volume else copy. Cross-target via `grove exec -- cargo … --target …`.

## Out of scope

Tasks, claims, fleets-as-product, verification ledgers, remote object caches, wrapping sccache/kache.
