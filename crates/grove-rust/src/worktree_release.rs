use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{cache, task};

use super::{
    find_lease, is_our_leased_worktree, preflight_salvage, remove_worktree, repo_context,
    repo_git_lock, salvage_work, worktree_acquire, worktree_materialize,
};

#[derive(Serialize)]
pub struct ReleaseOutcome {
    pub path: String,
    pub branch: String,
    /// The ref the worktree's uncommitted work was salvaged to, if it had any.
    pub saved_to: Option<String>,
}

/// Salvage and remove a leased worktree, refusing identity drift or live lanes.
pub fn release(root: &Path, target: &Path) -> Result<ReleaseOutcome> {
    release_except(root, target, None)
}

fn cleanup_ready(root: &Path, lease: &super::Lease) -> Result<()> {
    worktree_acquire::cleanup_ready(root, lease)?;
    worktree_materialize::cleanup_ready(root, lease)
}

fn same_authority(initial: &super::Lease, current: &super::Lease) -> bool {
    initial.workspace == current.workspace
        && initial.branch == current.branch
        && initial.agent == current.agent
        && initial.toolchain == current.toolchain
        && initial.repo == current.repo
        && initial.created_at == current.created_at
        && initial.generation == current.generation
        && initial.base_oid == current.base_oid
}

fn locked_ready(
    root: &Path,
    lease: &super::Lease,
    path: &Path,
    ignore_task: Option<&str>,
) -> Result<task::Blockers> {
    cleanup_ready(root, lease)?;
    if path.exists() && !is_our_leased_worktree(lease) {
        bail!(
            "{} is no longer grove's leased worktree; lease preserved, refusing to remove it",
            path.display()
        );
    }
    let blockers = task::blockers_except(root, &lease.repo, path, ignore_task)?;
    if !blockers.ids().is_empty() {
        bail!(
            "nonterminal tasks block release: {}",
            blockers.ids().join(", ")
        )
    }
    Ok(blockers)
}

pub(super) fn locked_preflight(
    root: &Path,
    lease: &super::Lease,
    path: &Path,
    ignore_task: Option<&str>,
) -> Result<task::Blockers> {
    let blockers = locked_ready(root, lease, path, ignore_task)?;
    if path.exists() {
        preflight_salvage(path, lease)?;
    }
    Ok(blockers)
}

pub(crate) fn preflight_except(
    root: &Path,
    target: &Path,
    ignore_task: Option<&str>,
) -> Result<()> {
    let ws = cache::canonical_path(target).to_string_lossy().into_owned();
    let (_, initial) = find_lease(root, &ws)?.with_context(|| {
        format!("no grove lease for {ws}; refusing to touch a worktree grove did not create")
    })?;
    let _lifecycle = cache::lifecycle_try_exclusive(root, &initial.repo, Path::new(&ws))?
        .with_context(|| format!("an active build or tagged lane holds {ws}; not releasing it"))?;
    let _git = repo_git_lock(root, &initial.repo)?;
    let (_, lease) = find_lease(root, &ws)?.with_context(|| {
        format!("no grove lease for {ws}; refusing to touch a worktree grove did not create")
    })?;
    if !same_authority(&initial, &lease) {
        bail!("lease authority for {ws} changed while acquiring cleanup locks")
    }
    let path = PathBuf::from(&ws);
    let blockers = locked_preflight(root, &lease, &path, ignore_task)?;
    drop(blockers);
    Ok(())
}

pub(crate) fn release_except(
    root: &Path,
    target: &Path,
    ignore_task: Option<&str>,
) -> Result<ReleaseOutcome> {
    let ws = cache::canonical_path(target).to_string_lossy().into_owned();
    let (_, initial) = find_lease(root, &ws)?.with_context(|| {
        format!("no grove lease for {ws}; refusing to touch a worktree grove did not create")
    })?;
    let _lifecycle = cache::lifecycle_try_exclusive(root, &initial.repo, Path::new(&ws))?
        .with_context(|| format!("an active build or tagged lane holds {ws}; not releasing it"))?;
    let _git = repo_git_lock(root, &initial.repo)?;
    let (lease_file, lease) = find_lease(root, &ws)?.with_context(|| {
        format!("no grove lease for {ws}; refusing to touch a worktree grove did not create")
    })?;
    if !same_authority(&initial, &lease) {
        bail!("lease authority for {ws} changed while acquiring cleanup locks")
    }
    let path = PathBuf::from(&ws);
    let blockers = locked_ready(root, &lease, &path, ignore_task)?;
    let mut saved_to = None;
    if path.exists() {
        let main_root = repo_context(&path)?.main_root;
        saved_to = salvage_work(&path, &lease)?;
        preflight_salvage(&path, &lease)?;
        remove_worktree(&main_root, &path)?;
    }
    drop(blockers);
    fs::remove_file(&lease_file)?;
    cache::reclaim_stale(root);
    Ok(ReleaseOutcome {
        path: ws,
        branch: lease.branch,
        saved_to,
    })
}
