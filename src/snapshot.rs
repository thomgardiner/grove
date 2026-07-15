//! Content-addressed workspace snapshots for verification evidence.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::cache;

const SCHEMA_VERSION: u32 = 1;

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
    pub entries: Vec<Entry>,
}

impl Snapshot {
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

/// Return every path whose tracked state, kind, mode, or content changed.
pub fn changed_paths(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let before: BTreeMap<_, _> = before
        .entries
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect();
    let after: BTreeMap<_, _> = after
        .entries
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect();
    before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .map(|path| (*path).clone())
        .collect()
}

/// Hash every tracked path and every non-ignored untracked path visible to Git.
/// Deleted tracked paths remain explicit entries, so absence is part of the identity.
pub fn capture(workspace: &Path) -> Result<Snapshot> {
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
    Ok(Snapshot {
        schema_version: SCHEMA_VERSION,
        sha256: digest(&entries),
        entries,
    })
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
    if snapshot.schema_version != SCHEMA_VERSION {
        bail!("unsupported verification snapshot schema")
    }
    if snapshot.sha256 != digest(&snapshot.entries) {
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
    output
        .stdout
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
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                bail!("git returned an unsafe repository path {path:?}");
            }
            Ok(path.to_string_lossy().into_owned())
        })
        .collect()
}

fn entry(workspace: &Path, path: &str, tracked: bool) -> Result<Entry> {
    let full = checked_path(workspace, path)?;
    let metadata = match fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && tracked => {
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
    Ok(format!(
        "{:x}",
        Sha256::digest(first.as_os_str().as_encoded_bytes())
    ))
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

fn digest(entries: &[Entry]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"grove.workspace-snapshot.v1\0");
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
