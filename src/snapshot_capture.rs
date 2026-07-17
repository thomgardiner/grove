use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use super::{Entry, INDEX_SCHEMA_VERSION, Kind, SCHEMA_VERSION, Snapshot};
use crate::cache;

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
    let index_tree = captured_index_tree(workspace)?;
    let head = captured_head(workspace)?;
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
        .map(|(path, tracked)| entry(workspace, &path, tracked))
        .collect();
    let entries = entries?;
    if captured_index_tree(workspace)? != index_tree || captured_head(workspace)? != head {
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
    let workspace = cache::canonical_path(workspace);
    for entry in &snapshot.entries {
        if entry.kind != Kind::Symlink {
            continue;
        }
        let link = checked_path(&workspace, &entry.path)?;
        let metadata = fs::symlink_metadata(&link)
            .with_context(|| format!("reading frozen source {}", link.display()))?;
        if !metadata.file_type().is_symlink() {
            bail!(
                "release source changed while validating symlink {}",
                entry.path
            )
        }
        let target = link_target(&link, &metadata)?;
        let actual = link_digest(&target);
        if entry.sha256.as_deref() != Some(actual.as_str()) {
            bail!(
                "release source changed while validating symlink {}",
                entry.path
            )
        }
        let target = relative_target(&workspace, &link, &target)?;
        let target = fs::canonicalize(&target).with_context(|| {
            format!("resolving frozen release symlink target for {}", entry.path)
        })?;
        if !target.starts_with(&workspace) {
            bail!(
                "frozen release refuses symlink {} whose target escapes the workspace",
                entry.path
            )
        }
    }
    Ok(())
}

fn listed(workspace: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
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

fn captured_index_tree(workspace: &Path) -> Result<String> {
    let _lock = INDEX_TREE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn entry(workspace: &Path, path: &str, tracked: bool) -> Result<Entry> {
    let full = checked_path(workspace, path)?;
    let metadata = match fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && tracked => {
            if let Some(entry) = snapshot_index::missing(workspace, path)? {
                return Ok(entry);
            }
            return Ok(Entry {
                path: path.into(),
                tracked,
                kind: Kind::Deleted,
                sha256: None,
                mode: None,
            });
        }
        Err(error) => return Err(error).with_context(|| format!("reading {path}")),
    };
    let (kind, sha256) = if metadata.file_type().is_symlink() {
        (Kind::Symlink, link_hash(&full, &metadata)?)
    } else if metadata.is_file() {
        (Kind::File, file_hash(&full, &metadata)?)
    } else {
        bail!("verification snapshot refuses non-file path {path}");
    };
    Ok(Entry {
        path: path.into(),
        tracked,
        kind,
        sha256: Some(sha256),
        mode: Some(mode(&metadata)),
    })
}

fn checked_path(workspace: &Path, path: &str) -> Result<PathBuf> {
    let parts: Vec<_> = Path::new(path).components().collect();
    let mut full = workspace.to_path_buf();
    let workspace = cache::canonical_path(workspace);
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(part) = part else {
            bail!("invalid verification snapshot path {path:?}")
        };
        full.push(part);
        if index + 1 == parts.len() {
            continue;
        }
        match fs::symlink_metadata(&full) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("verification snapshot refuses symlinked parent {path:?}")
            }
            Ok(metadata) => {
                if metadata.is_dir()
                    && !fs::canonicalize(&full)
                        .with_context(|| format!("resolving {}", full.display()))?
                        .starts_with(&workspace)
                {
                    bail!("verification snapshot refuses parent outside the workspace")
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error).with_context(|| format!("reading {}", full.display())),
        }
    }
    Ok(full)
}

fn file_hash(path: &Path, before: &Metadata) -> Result<String> {
    let first = read_hash(path)?;
    let after =
        fs::symlink_metadata(path).with_context(|| format!("rechecking {}", path.display()))?;
    if !stable(before, &after) {
        bail!("workspace changed while hashing {}", path.display());
    }
    let second = read_hash(path)?;
    if first != second {
        bail!("workspace changed while hashing {}", path.display());
    }
    Ok(first)
}

fn read_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let count = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if count == 0 {
            break;
        }
        hash.update(&buf[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn link_hash(path: &Path, before: &Metadata) -> Result<String> {
    Ok(link_digest(&link_target(path, before)?))
}

fn link_target(path: &Path, before: &Metadata) -> Result<PathBuf> {
    let first =
        fs::read_link(path).with_context(|| format!("reading symlink {}", path.display()))?;
    let after = fs::symlink_metadata(path)
        .with_context(|| format!("rechecking symlink {}", path.display()))?;
    if !stable(before, &after) {
        bail!("workspace changed while hashing {}", path.display())
    }
    let second =
        fs::read_link(path).with_context(|| format!("rechecking symlink {}", path.display()))?;
    if first != second {
        bail!("workspace changed while hashing {}", path.display())
    }
    Ok(first)
}

fn link_digest(target: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(target.as_os_str().as_encoded_bytes())
    )
}

fn relative_target(workspace: &Path, link: &Path, target: &Path) -> Result<PathBuf> {
    if target.is_absolute() {
        bail!(
            "frozen release refuses symlink {} with an absolute target",
            link.display()
        )
    }
    let mut resolved = link
        .parent()
        .context("frozen release symlink has no parent")?
        .to_path_buf();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => resolved.push(part),
            Component::ParentDir => {
                if !resolved.pop() || !resolved.starts_with(workspace) {
                    bail!(
                        "frozen release refuses symlink {} whose target escapes the workspace",
                        link.display()
                    )
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!(
                    "frozen release refuses symlink {} with an absolute target",
                    link.display()
                )
            }
        }
    }
    if !resolved.starts_with(workspace) {
        bail!(
            "frozen release refuses symlink {} whose target escapes the workspace",
            link.display()
        )
    }
    Ok(resolved)
}

#[cfg(unix)]
fn stable(before: &Metadata, after: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && mode(before) == mode(after)
        && before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.file_type() == after.file_type()
}

#[cfg(not(unix))]
fn stable(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && mode(before) == mode(after)
        && before.file_type() == after.file_type()
}

#[cfg(unix)]
fn mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn mode(metadata: &Metadata) -> u32 {
    if metadata.permissions().readonly() {
        1
    } else {
        0
    }
}
