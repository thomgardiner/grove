# Changelog

## 0.1.0 — 2026-08-08

Grove is a local Cargo host for Rust repositories. It keeps one warm seed
target per repository and toolchain, reuses seed artifacts in worktree lanes,
and routes scoped Cargo commands through those targets.

- `grove warm` fills the primary checkout's seed; `grove check` covers dirty
  package changes and `grove check --base auto` proves changes against trunk.
- `grove build`, `grove clippy`, `grove test`, and `grove exec` use the
  checkout's seed or lane; `grove env` exports the selected target.
- `grove worktree acquire` creates repo-adjacent stamped worktrees, and
  `grove worktree release` keeps branches while refusing dirty removal unless
  `--force` is supplied.
- Cache hygiene removes dead stamped lanes and applies the configured cache
  ceiling. Grove does not run tasks, provide remote/shared compilation, or
  wrap sccache.
