//! Rust observations for generic stale-task recovery.

use anyhow::Result;
use std::path::Path;

pub use grove_core::recovery::{ReapReport, ReapedTask, SkippedTask};

use crate::{cache, project, worktree};

/// Reap stale tasks after supplying Cargo-lane and sparse-worktree observations
/// to the language-neutral recovery authority.
pub fn reap(root: &Path, workspace: &Path, ttl: u64, dry_run: bool) -> Result<ReapReport> {
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    grove_core::recovery::reap(
        root,
        &repo,
        ttl,
        dry_run,
        |task| cache::workspace_busy(root, &task.workspace, None),
        |task| {
            let workspace = Path::new(&task.workspace);
            let leased = worktree::managed(root, workspace)?;
            if leased {
                worktree::preflight_except(root, workspace, Some(&task.id))?;
            }
            Ok(leased)
        },
        |task| {
            worktree::release_except(root, Path::new(&task.workspace), Some(&task.id))
                .map(|outcome| outcome.saved_to)
        },
    )
}
