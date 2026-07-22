# Changelog

All notable changes to Grove are documented here. Grove follows semantic versioning.

## 0.3.5 — unreleased

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
