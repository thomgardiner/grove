//! Local cargo host: seed target, impact-scoped checks, hardlink worktree lanes.
//!
//! ```ignore
//! use grove::{CacheHost, WorktreeManager, cargo_check};
//!
//! let host = CacheHost::open(repo)?;
//! host.warm(repo)?;
//! cargo_check(repo, "HEAD", None)?;
//! let wt = WorktreeManager::open(repo)?.acquire("spike", None, None, false)?;
//! cargo_check(&wt, "HEAD", None)?;
//! ```

pub mod cache;
pub mod cargo_cmd;
mod git;
pub mod impact;
pub mod skill;
mod util;
pub mod worktree;

#[cfg(test)]
mod host_tests;

pub use cache::{
    CacheHost, CacheStatus, CleanOpts, CleanReport, Lane, LaneKind, cache_root, format_bytes,
    live_lane_keys, repo_slug,
};
pub use cargo_cmd::{
    CargoVerb, cargo_build, cargo_check, cargo_clippy, cargo_impact, cargo_test, env_exports, exec,
    warm,
};
pub use impact::{Plan, plan, resolve_base};
pub use skill::{SKILL_MD, describe as skill_describe, install as skill_install, print_skill};
pub use worktree::{
    ReleaseOutcome, WorktreeInfo, WorktreeManager, default_worktree_parent, worktree_parent,
};
