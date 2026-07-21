use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::process::Command;
use std::sync::Mutex;

use super::{INDEX_SCHEMA_VERSION, SCHEMA_VERSION, Snapshot};

#[path = "snapshot_capture_filesystem.rs"]
mod filesystem;
#[path = "snapshot_read_only.rs"]
mod read_only;
#[path = "snapshot_index.rs"]
mod snapshot_index;

// ponytail: process-global serialization is negligible beside hashing/builds; shard by index path only if profiling proves contention.
// `git write-tree` takes `.git/index.lock`, so parallel DAG workers need an in-process gate.
static INDEX_TREE_LOCK: Mutex<()> = Mutex::new(());

/// Hash every tracked path and every non-ignored untracked path visible to Git, plus
/// the exact staged index tree and `HEAD`. Deleted tracked paths remain explicit entries,
/// so absence, staged-vs-unstaged content, and Git-aware command inputs are all part of
/// the identity.
pub(super) fn capture(workspace: &Path) -> Result<Snapshot> {
    capture_with(workspace, false)
}

pub(super) fn capture_read_only(workspace: &Path) -> Result<Snapshot> {
    capture_with(workspace, true)
}

fn capture_with(workspace: &Path, read_only: bool) -> Result<Snapshot> {
    let index_tree = captured_index_tree(workspace, read_only)?;
    let head = captured_head(workspace)?;
    let gitlinks = snapshot_index::gitlinks(workspace)?;
    let mut paths = BTreeMap::new();
    for path in listed(workspace, &["ls-files", "--cached", "-z"])? {
        paths.insert(path, true);
    }
    for path in listed(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )? {
        paths.entry(path).or_insert(false);
    }
    let entries: Result<Vec<_>> = paths
        .into_iter()
        .map(|(path, tracked)| filesystem::entry(workspace, &path, tracked, gitlinks.get(&path)))
        .collect();
    let entries = entries?;
    if captured_index_tree(workspace, read_only)? != index_tree || captured_head(workspace)? != head
    {
        bail!("Git state changed while hashing the workspace")
    }
    let schema_version = if head.is_some() {
        SCHEMA_VERSION
    } else {
        INDEX_SCHEMA_VERSION
    };
    Ok(Snapshot {
        schema_version,
        sha256: super::digest(&entries, Some(&index_tree), head.as_deref()),
        index_tree: Some(index_tree),
        head,
        entries,
    })
}

pub(super) fn changed_index_paths(
    workspace: &Path,
    before_tree: &str,
    after_tree: &str,
) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "diff-tree",
            "--no-commit-id",
            "--no-renames",
            "-r",
            "--name-only",
            "-z",
            before_tree,
            after_tree,
        ])
        .current_dir(workspace)
        .output()
        .context("listing staged Git index changes")?;
    if !output.status.success() {
        bail!(
            "listing staged Git index changes failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    repository_paths(&output.stdout)
}

/// Refuse symlink targets that could reach outside a frozen release worktree.
pub(super) fn validate_frozen_links(workspace: &Path, snapshot: &Snapshot) -> Result<()> {
    filesystem::validate_frozen_links(workspace, snapshot)
}

fn listed(workspace: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("spawning git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    repository_paths(&output.stdout)
}

fn repository_paths(bytes: &[u8]) -> Result<Vec<String>> {
    bytes
        .split(|byte| *byte == b'\0')
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = std::str::from_utf8(path)
                .context("verification snapshots require UTF-8 repository paths")?;
            if path.contains('\\') {
                bail!("verification snapshots reject backslash repository paths");
            }
            let path = Path::new(path);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            {
                bail!("git returned an unsafe repository path {path:?}");
            }
            Ok(path.to_string_lossy().into_owned())
        })
        .collect()
}

fn captured_index_tree(workspace: &Path, read_only: bool) -> Result<String> {
    let _lock = INDEX_TREE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if read_only {
        read_only_index_tree(workspace)
    } else {
        write_index_tree(workspace)
    }
}

fn read_only_index_tree(workspace: &Path) -> Result<String> {
    read_only::index_tree(workspace)
}

fn write_index_tree(workspace: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["write-tree"])
        .current_dir(workspace)
        .output()
        .context("capturing Git index tree")?;
    if !output.status.success() {
        bail!(
            "capturing Git index tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Git returned an invalid index tree")
    }
    Ok(tree)
}

fn captured_head(workspace: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("capturing Git HEAD")?;
    if !output.status.success() {
        return Ok(None);
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !matches!(head.len(), 40 | 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Git returned an invalid HEAD")
    }
    Ok(Some(head))
}
