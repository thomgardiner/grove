//! Working-tree overlay for exact inspection snapshots.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::snapshot;

pub(super) fn validate(source: &Path) -> Result<()> {
    validate_dir(source, source)
}

pub(super) fn validate_links(source: &Path, snapshot: &snapshot::Snapshot) -> Result<()> {
    let entries: BTreeMap<_, _> = snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    for entry in snapshot
        .entries
        .iter()
        .filter(|entry| entry.kind == snapshot::Kind::Symlink)
    {
        validate_link(source, PathBuf::from(&entry.path), &entries)?;
    }
    Ok(())
}

fn validate_link(
    source: &Path,
    mut relative: PathBuf,
    entries: &BTreeMap<String, &snapshot::Entry>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    loop {
        let key = path_key(&relative)?;
        if !seen.insert(key.clone()) {
            bail!("inspection snapshot rejects a symlink cycle")
        }
        let entry = entries.get(&key).with_context(|| {
            format!(
                "inspection symlink target is absent from the snapshot: {}",
                relative.display()
            )
        })?;
        match entry.kind {
            snapshot::Kind::File => return validate_link_file(source, &relative),
            snapshot::Kind::Deleted => bail!(
                "inspection symlink target is deleted in the snapshot: {}",
                relative.display()
            ),
            snapshot::Kind::Symlink => {
                let link = source.join(&relative);
                let target = fs::read_link(&link)
                    .with_context(|| format!("reading inspection symlink {}", link.display()))?;
                relative = resolve_link(&relative, &target)?;
            }
        }
    }
}

fn path_key(path: &Path) -> Result<String> {
    path.components()
        .map(|component| match component {
            Component::Normal(part) => part
                .to_str()
                .context("inspection symlink target is not UTF-8"),
            _ => bail!("inspection symlink target is not repository-relative"),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn validate_link_file(source: &Path, relative: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source.join(relative))?;
    if !regular(&metadata) {
        bail!(
            "inspection symlink target is not a captured regular file: {}",
            relative.display()
        )
    }
    Ok(())
}

fn resolve_link(link: &Path, target: &Path) -> Result<PathBuf> {
    if target.is_absolute() {
        bail!("inspection snapshot rejects an absolute symlink target")
    }
    let mut resolved = link.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => resolved.push(part),
            Component::ParentDir if resolved.pop() => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                bail!("inspection snapshot rejects an escaping symlink target")
            }
        }
    }
    if resolved.as_os_str().is_empty() {
        bail!("inspection snapshot rejects a directory symlink target")
    }
    Ok(resolved)
}

pub(super) fn overlay(source: &Path, capsule: &Path, start: &snapshot::Snapshot) -> Result<()> {
    for entry in &start.entries {
        let relative = relative(&entry.path)?;
        let input = source.join(&relative);
        let output = capsule.join(&relative);
        parent(capsule, &relative)?;
        match fs::symlink_metadata(&input) {
            Ok(metadata) => copy(&input, &output, entry, &metadata)?,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && entry.kind == snapshot::Kind::Deleted => {}
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && entry.tracked
                    && entry.kind == snapshot::Kind::File =>
            {
                bail!("inspection source has unsupported sparse index state")
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading inspection source {}", input.display()));
            }
        }
    }
    Ok(())
}

fn copy(
    source: &Path,
    destination: &Path,
    entry: &snapshot::Entry,
    metadata: &fs::Metadata,
) -> Result<()> {
    if entry.kind == snapshot::Kind::Deleted {
        bail!("inspection source changed at {}", entry.path)
    }
    match entry.kind {
        snapshot::Kind::File if regular(metadata) => {
            let mut input = File::open(source)
                .with_context(|| format!("opening inspection source {}", source.display()))?;
            let mut output = create(destination)?;
            io::copy(&mut input, &mut output)
                .with_context(|| format!("copying inspection source {}", source.display()))?;
            output.sync_all()?;
            permissions(destination, entry.mode)?;
        }
        snapshot::Kind::Symlink if metadata.file_type().is_symlink() => {
            let target = fs::read_link(source)
                .with_context(|| format!("reading inspection symlink {}", source.display()))?;
            link(&target, source, destination)?;
        }
        _ => bail!("inspection source changed at {}", entry.path),
    }
    Ok(())
}

fn parent(root: &Path, relative: &Path) -> Result<()> {
    let parent = relative
        .parent()
        .context("inspection entry has no parent")?;
    let mut current = root.to_path_buf();
    for part in parent.components() {
        let Component::Normal(part) = part else {
            bail!("invalid inspection entry parent")
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if real_dir(&metadata) => {}
            Ok(_) => bail!("inspection entry parent is not a real directory"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!("creating inspection directory {}", current.display())
                })?;
            }
            Err(error) => return Err(error).context("reading inspection entry parent"),
        }
    }
    Ok(())
}

fn relative(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid inspection snapshot path {path:?}")
    }
    Ok(path.to_path_buf())
}

fn validate_dir(source: &Path, dir: &Path) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading inspection source {}", dir.display()))?
    {
        let entry = entry?;
        if dir == source && entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("reading inspection source {}", path.display()))?;
        if real_dir(&metadata) {
            validate_dir(source, &path)?;
        } else if !regular(&metadata) && !metadata.file_type().is_symlink() {
            bail!("inspection source contains special file {}", path.display())
        }
    }
    Ok(())
}

fn create(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating inspection entry {}", path.display()))
}

#[cfg(unix)]
fn permissions(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = mode.context("inspection file mode is missing")?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn permissions(path: &Path, mode: Option<u32>) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode.context("inspection file mode is missing")? == 1);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(unix)]
fn link(target: &Path, _source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)
        .with_context(|| format!("creating inspection symlink {}", destination.display()))
}

#[cfg(windows)]
fn link(target: &Path, source: &Path, destination: &Path) -> Result<()> {
    let resolved = source
        .parent()
        .context("inspection symlink has no parent")?
        .join(target);
    if fs::metadata(resolved)?.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

fn regular(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink() && !reparse(metadata)
}

fn real_dir(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink() && !reparse(metadata)
}

#[cfg(windows)]
fn reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn reparse(_metadata: &fs::Metadata) -> bool {
    false
}
