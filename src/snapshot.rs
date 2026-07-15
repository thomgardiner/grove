//! Content-addressed workspace snapshots for verification evidence.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crate::cache;

pub(super) const SCHEMA_VERSION: u32 = 3;
pub(super) const LEGACY_SCHEMA_VERSION: u32 = 1;
pub(super) const INDEX_SCHEMA_VERSION: u32 = 2;

#[path = "snapshot_capture.rs"]
mod capture;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Symlink,
    Deleted,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub tracked: bool,
    pub kind: Kind,
    pub sha256: Option<String>,
    pub mode: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Ref {
    pub sha256: String,
    pub entries: usize,
    pub tracked: usize,
    pub untracked: usize,
    pub deleted: usize,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub schema_version: u32,
    pub sha256: String,
    #[serde(default)]
    index_tree: Option<String>,
    #[serde(default)]
    head: Option<String>,
    pub entries: Vec<Entry>,
}

impl Snapshot {
    /// The exact Git index tree captured with these working-tree entries. A frozen
    /// release uses it rather than re-reading a mutable source index.
    pub(crate) fn index_tree(&self) -> Result<&str> {
        self.index_tree
            .as_deref()
            .context("verification snapshot lacks a captured Git index tree")
    }

    /// The exact commit checked out when this snapshot was captured. Frozen release
    /// needs it because Git-aware verification commands can consume HEAD directly.
    pub(crate) fn head(&self) -> Result<&str> {
        self.head
            .as_deref()
            .context("verification snapshot lacks a captured Git HEAD")
    }

    pub fn reference(&self) -> Ref {
        let mut tracked = 0;
        let mut untracked = 0;
        let mut deleted = 0;
        for entry in &self.entries {
            if entry.tracked {
                tracked += 1;
            } else {
                untracked += 1;
            }
            if entry.kind == Kind::Deleted {
                deleted += 1;
            }
        }
        Ref {
            sha256: self.sha256.clone(),
            entries: self.entries.len(),
            tracked,
            untracked,
            deleted,
        }
    }
}

/// Cooperatively serialize operations that inspect or verify one worktree. External
/// writers are still caught by the before/after snapshots; this closes Grove races.
pub fn workspace_lock(root: &Path, workspace: &Path) -> Result<File> {
    let workspace = cache::canonical_path(workspace);
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    let file = File::create(locks.join(format!(
        "snapshot-workspace-{}.lock",
        cache::repo_slug(&workspace.to_string_lossy())
    )))
    .context("opening workspace snapshot lock")?;
    file.lock_exclusive()
        .context("locking workspace snapshot")?;
    Ok(file)
}

/// Return every path whose working-tree or staged-index state changed.
pub fn changed_paths(workspace: &Path, before: &Snapshot, after: &Snapshot) -> Result<Vec<String>> {
    let before_tree = before
        .index_tree()
        .context("task scope snapshot lacks a captured Git index tree")?;
    let after_tree = after
        .index_tree()
        .context("current task workspace lacks a captured Git index tree")?;
    let before_entries: BTreeMap<_, _> = before
        .entries
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect();
    let after_entries: BTreeMap<_, _> = after
        .entries
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect();
    let paths: BTreeSet<_> = before_entries.keys().chain(after_entries.keys()).collect();
    let mut paths: BTreeSet<String> = paths
        .into_iter()
        .filter(|path| before_entries.get(*path) != after_entries.get(*path))
        .map(|path| (*path).clone())
        .collect();
    if before_tree != after_tree {
        paths.extend(capture::changed_index_paths(
            workspace,
            before_tree,
            after_tree,
        )?);
    }
    Ok(paths.into_iter().collect())
}

pub fn capture(workspace: &Path) -> Result<Snapshot> {
    capture::capture(workspace)
}

/// Reject release symlinks whose resolved target is outside the captured workspace.
pub(crate) fn validate_frozen_links(workspace: &Path, snapshot: &Snapshot) -> Result<()> {
    capture::validate_frozen_links(workspace, snapshot)
}

/// Persist the complete manifest once under its own digest, then return the compact receipt form.
pub fn persist(root: &Path, repo: &str, snapshot: &Snapshot) -> Result<Ref> {
    let reference = snapshot.reference();
    let dir = root.join("snapshots").join(cache::repo_slug(repo));
    let path = manifest_path(&dir, &snapshot.sha256)?;
    let _lock = manifest_lock(root, repo)?;
    if !path.exists() {
        cache::write_atomic(&path, &serde_json::to_vec_pretty(snapshot)?)?;
    } else {
        validate(root, repo, &reference)?;
    }
    Ok(reference)
}

fn manifest_lock(root: &Path, repo: &str) -> Result<File> {
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    let path = locks.join(format!("snapshot-manifest-{}.lock", cache::repo_slug(repo)));
    let file = File::create(&path).with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// Load a receipt's manifest and prove it still names exactly the recorded content.
/// Receipt metadata alone is not evidence: its sidecar must exist and re-hash cleanly.
pub fn validate(root: &Path, repo: &str, reference: &Ref) -> Result<Snapshot> {
    let dir = root.join("snapshots").join(cache::repo_slug(repo));
    let path = manifest_path(&dir, &reference.sha256)?;
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if !matches!(
        snapshot.schema_version,
        LEGACY_SCHEMA_VERSION | INDEX_SCHEMA_VERSION | SCHEMA_VERSION
    ) {
        bail!("unsupported verification snapshot schema")
    }
    if snapshot.schema_version >= INDEX_SCHEMA_VERSION && snapshot.index_tree.is_none() {
        bail!("verification snapshot lacks a Git index tree")
    }
    if snapshot.schema_version == LEGACY_SCHEMA_VERSION
        && (snapshot.index_tree.is_some() || snapshot.head.is_some())
    {
        bail!("legacy verification snapshot unexpectedly has a Git index tree")
    }
    if snapshot.schema_version == INDEX_SCHEMA_VERSION && snapshot.head.is_some() {
        bail!("index verification snapshot unexpectedly has a Git HEAD")
    }
    if snapshot.schema_version == SCHEMA_VERSION && snapshot.head.is_none() {
        bail!("verification snapshot lacks a Git HEAD")
    }
    if snapshot.sha256
        != digest(
            &snapshot.entries,
            snapshot.index_tree.as_deref(),
            snapshot.head.as_deref(),
        )
    {
        bail!("verification snapshot digest does not match its entries")
    }
    if snapshot.reference() != *reference {
        bail!("verification snapshot does not match its receipt reference")
    }
    Ok(snapshot)
}

fn manifest_path(dir: &Path, sha256: &str) -> Result<PathBuf> {
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid verification snapshot digest")
    }
    Ok(dir.join(format!("{sha256}.json")))
}

fn digest(entries: &[Entry], index_tree: Option<&str>, head: Option<&str>) -> String {
    let mut hash = Sha256::new();
    match (index_tree, head) {
        (Some(tree), Some(head)) => {
            hash.update(b"grove.workspace-snapshot.v3\0");
            hash.update(tree.as_bytes());
            hash.update([0]);
            hash.update(head.as_bytes());
            hash.update([0]);
        }
        (Some(tree), None) => {
            hash.update(b"grove.workspace-snapshot.v2\0");
            hash.update(tree.as_bytes());
            hash.update([0]);
        }
        (None, None) => hash.update(b"grove.workspace-snapshot.v1\0"),
        (None, Some(_)) => unreachable!("HEAD requires an index tree"),
    }
    for entry in entries {
        hash.update(entry.path.as_bytes());
        hash.update([0]);
        hash.update([u8::from(entry.tracked)]);
        hash.update([match entry.kind {
            Kind::File => 1,
            Kind::Symlink => 2,
            Kind::Deleted => 3,
        }]);
        hash.update(entry.mode.unwrap_or_default().to_le_bytes());
        hash.update(entry.sha256.as_deref().unwrap_or_default().as_bytes());
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn snapshot(workspace: &Path) -> Snapshot {
        capture(workspace).unwrap()
    }

    #[test]
    fn frozen_release_links_stay_inside_the_workspace() {
        use std::os::unix::fs::symlink;

        let base = tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let outside = base.path().join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&outside).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
        fs::write(workspace.join("inside"), "inside").unwrap();

        symlink("inside", workspace.join("link")).unwrap();
        assert!(validate_frozen_links(&workspace, &snapshot(&workspace)).is_ok());

        fs::remove_file(workspace.join("link")).unwrap();
        symlink(&outside, workspace.join("link")).unwrap();
        assert!(validate_frozen_links(&workspace, &snapshot(&workspace)).is_err());

        fs::remove_file(workspace.join("link")).unwrap();
        symlink("../outside", workspace.join("link")).unwrap();
        assert!(validate_frozen_links(&workspace, &snapshot(&workspace)).is_err());

        fs::remove_file(workspace.join("link")).unwrap();
        symlink(&outside, workspace.join("alias")).unwrap();
        symlink("alias", workspace.join("link")).unwrap();
        assert!(validate_frozen_links(&workspace, &snapshot(&workspace)).is_err());
    }
}
