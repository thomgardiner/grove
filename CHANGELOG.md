# Changelog

All notable changes to Grove are documented here. Grove follows semantic versioning.

## 0.3.3 — unreleased

### Added

- Durable task-status schema 2 exposes the verification state recorded when a task finishes.
- Self-hosting qualification exercises Grove-managed worktrees, tasks, receipts, and cleanup.
- Binary archives, checksums, shell and PowerShell installers, and an updater are generated for
  GitHub Releases.

### Changed

- Task lifecycle readiness uses direct synchronization instead of polling indirect lane state.
- Grove's core coordination and Rust acceleration boundaries are documented explicitly.
- Release distribution is binary-only; the workspace crates are not published to crates.io.

### Fixed

- Legacy task cleanup remains compatible after schema 2 verification evidence was introduced.

## 0.3.2

- Added verification receipts, frozen release bundles, portable evidence queries, and hardened
  worktree/task recovery.
