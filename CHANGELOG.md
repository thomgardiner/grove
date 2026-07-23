# Changelog

All notable changes to Grove are documented here. Grove follows semantic versioning.

## Unreleased

### Added

- `grove git -- <args>` serializes the git writes that race concurrent worktrees
  on shared `.git` state (config, tags, refs), the most-reported multi-agent
  failure, which worktrees do not fix. It runs under the same lock grove's
  worktree plumbing uses, locks only shared-state writers, and returns git's
  exit code. `grove task exec` routes a supervised command's git through it
  automatically via a PATH shim (Unix), so a fleet gets safe git for free.
- `capabilities` reports `coordination.git_write_serialization`.

- `grove mcp serve` speaks the Model Context Protocol over stdio, exposing claims,
  tasks, status, and worktrees as tools. Any MCP-client harness (seven of the eight
  major agent CLIs) coordinates through grove without shell access, which is what
  makes mixed-vendor fleets against one repository workable. Tools-only by design.
- `grove init` writes a `CLAUDE.md` bridge (one `@AGENTS.md` import line) when the
  repository lacks one, because Claude Code reads `CLAUDE.md` and not the standard
  contract filename. The contract stays single-sourced in `AGENTS.md`.
- `capabilities` reports `coordination.mcp_tools` and
  `coordination.agent_identity_required`.

### Fixed

- `task finish` no longer refuses over a build-generated workspace-root
  `Cargo.lock`. A fresh crate commits no lockfile, so its first build leaves an
  untracked `Cargo.lock` at the root that no crate-level scope covers; finish
  reported it as an out-of-scope write and blocked every fresh binary crate.
  The lockfile is Cargo's build byproduct, not an agent write, so it is exempt
  from the scope check in a Cargo workspace.

### Changed

- BREAKING: `claim`, `worktree acquire`, and `release claims` no longer default
  `--agent` to the shared name "agent". Claim identity is hash(agent, scope) and a
  same-identity claim is a renewal, so two sessions using the default silently took
  over each other's claims instead of conflicting. Pass `--agent <stable-id>` or
  set `GROVE_AGENT` once per session.

## 0.3.5 — 2026-07-22

### Added

- `task exec --capability edit` supervises a command without reserving a build lane or an
  admission slot, so long-lived agent sessions no longer consume `max_builders`. The default
  `build` capability keeps the previous behavior.
- `task begin` pins the verification-policy digest; `task finish` refuses with `policy_changed`
  when the policy moved, and accepts a reviewed change via `--accept-policy <sha256>`.
- `capabilities` reports `task.exec_capabilities` and `task.verification_policy_pinned`.
- `why-rebuilt` reports how many units Cargo reused versus recompiled and explains each stale
  unit. `--fresh` seeds a throwaway lane from the canonical and discards it, so a cache that
  has stopped seeding is a visible number instead of an unexplained slowdown.
- `check` and `test` print what they routed, how long it took, and whether the lane was warm,
  seeded from the canonical, or the unverified bootstrap fallback. Elapsed time only; Grove has
  no counterfactual for plain Cargo and does not claim time saved.
- `cache status` reports `healthy`, `headroom_bytes`, and `busy_lane_count`, so lane inventory
  can be read against the eviction floor instead of against logical sizes that overcount
  copy-on-write sharing.

### Changed

- Task record schema 6 adds `verification_policy_sha256`; schema 5 records migrate with no
  pinned policy and are evaluated exactly as before.
- Every fallback to the serialized bootstrap lane now says so and why. Previously only `exec`
  warned, so an unwarmed repo made `check` and `test` silently cold.
- A grove build launched inside a lane-holding `task exec` refuses immediately with a
  diagnostic instead of blocking until the task deadline. This also applies to verification
  commands, which run in a lane: one that deliberately invoked `grove exec` or `grove check`
  used to deadlock and now fails fast.

## 0.3.4 — 2026-07-21

### Added

- Cache eligibility inspection, strict copy-on-write capability probing, and deterministic
  head-to-head benchmark fixtures with behavior receipts.
- Cache keys bound to the resolved compiler identity, preventing warm-state reuse across
  different compiler builds selected by the same toolchain label.
- Durable task-status schema 2 exposes the verification state recorded when a task finishes.
- Self-hosting qualification exercises Grove-managed worktrees, tasks, receipts, and cleanup.
- Binary archives, checksums, shell and PowerShell installers, and an updater are generated for
  GitHub Releases.

### Changed

- Task lifecycle readiness uses direct synchronization instead of polling indirect lane state.
- Grove's core coordination and Rust acceleration boundaries are documented explicitly.
- Release distribution is binary-only; the workspace crates are not published to crates.io.

### Fixed

- GNU jobserver coordination now supports cache roots containing spaces.
- Canonical seed publication is policy-bound and build-script repair is atomic.
- Legacy task cleanup remains compatible after schema 2 verification evidence was introduced.

## 0.3.2

- Added verification receipts, frozen release bundles, portable evidence queries, and hardened
  worktree/task recovery.
