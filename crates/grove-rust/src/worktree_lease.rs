use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::cache;

use super::MaterializationRecord;

pub type Lease = grove_core::worktree::Lease<MaterializationRecord>;

pub(super) fn generation() -> String {
    grove_core::worktree::generation()
}

pub(super) fn leases(root: &Path) -> Vec<(PathBuf, Lease)> {
    grove_core::worktree::leases(root)
}

pub(super) fn find_lease(root: &Path, workspace: &str) -> Result<Option<(PathBuf, Lease)>> {
    grove_core::worktree::find_lease(root, workspace)
}

pub(super) fn containing(root: &Path, target: &Path) -> Result<Option<(PathBuf, Lease)>> {
    grove_core::worktree::containing(root, target)
}

pub(super) fn write_lease(root: &Path, lease: &Lease) -> Result<()> {
    grove_core::worktree::write_lease_named(
        root,
        &cache::lane_id(&lease.workspace, &lease.toolchain),
        lease,
    )
}

pub(super) fn activity(root: &Path, lease: &Lease) -> u64 {
    grove_core::worktree::activity(lease)
        .max(cache::workspace_last_used(root, &lease.workspace).unwrap_or(0))
}
