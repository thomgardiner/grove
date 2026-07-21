use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use super::super::{Entry, Kind, Snapshot};
use super::snapshot_index;
use crate::canonical_path;

/// Refuse symlink targets that could reach outside a frozen release worktree.
pub(super) fn validate_frozen_links(workspace: &Path, snapshot: &Snapshot) -> Result<()> {
    let workspace = canonical_path(workspace);
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

pub(super) fn entry(
    workspace: &Path,
    path: &str,
    tracked: bool,
    gitlink: Option<&Entry>,
) -> Result<Entry> {
    let full = checked_path(workspace, path)?;
    if let Some(entry) = gitlink {
        validate_uninitialized_gitlink(&full, path)?;
        return Ok(entry.clone());
    }
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

fn validate_uninitialized_gitlink(full: &Path, path: &str) -> Result<()> {
    let metadata = match fs::symlink_metadata(full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading gitlink {path}")),
    };
    if metadata.is_dir()
        && fs::read_dir(full)
            .with_context(|| format!("reading gitlink {path}"))?
            .next()
            .transpose()?
            .is_none()
    {
        return Ok(());
    }
    bail!("verification snapshot refuses initialized submodule path {path}")
}

fn checked_path(workspace: &Path, path: &str) -> Result<PathBuf> {
    let parts: Vec<_> = Path::new(path).components().collect();
    let mut full = workspace.to_path_buf();
    let workspace = canonical_path(workspace);
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
    Ok(crate::hex(&hash.finalize()))
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
    crate::hex(&Sha256::digest(target.as_os_str().as_encoded_bytes()))
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
