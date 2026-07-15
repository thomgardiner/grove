//! Isolated worktree materialization for frozen release verification.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, snapshot};

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A detached Git worktree whose visible files exactly match the captured snapshot.
/// Dropping it removes only this worktree and its one-use release lane can then be
/// discarded independently.
pub(super) struct FrozenWorkspace {
    source: PathBuf,
    path: PathBuf,
}

impl FrozenWorkspace {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FrozenWorkspace {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .current_dir(&self.source)
            .status();
        if self.path.exists() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Start from `HEAD`, copy the source index, then replace all visible files with the
/// captured bytes. The final capture is the proof that staged, dirty, untracked, and
/// deleted paths survived the transfer exactly.
pub(super) fn materialize(
    root: &Path,
    source: &Path,
    start: &snapshot::Snapshot,
) -> Result<FrozenWorkspace> {
    let path = scratch_path(root, source)?;
    let output = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg("HEAD")
        .current_dir(source)
        .output()
        .context("creating frozen release worktree")?;
    if !output.status.success() {
        bail!(
            "creating frozen release worktree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let frozen = FrozenWorkspace {
        source: source.to_path_buf(),
        path,
    };
    mirror_index(source, frozen.path())?;
    clear_worktree(frozen.path())?;
    for entry in &start.entries {
        if entry.kind != snapshot::Kind::Deleted {
            copy_entry(source, frozen.path(), entry)?;
        }
    }
    if snapshot::capture(frozen.path())? != *start {
        bail!("could not materialize the captured release snapshot")
    }
    Ok(frozen)
}

fn scratch_path(root: &Path, source: &Path) -> Result<PathBuf> {
    let root = cache::canonical_path(root);
    let base = if root.starts_with(source) {
        std::env::temp_dir().join("grove-release-worktrees")
    } else {
        root.join("release-worktrees")
    }
    .join(cache::repo_slug(&source.to_string_lossy()));
    fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for _ in 0..64 {
        let sequence = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("freeze-{nanos:x}-{sequence:x}"));
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!("could not allocate a frozen release worktree path")
}

fn mirror_index(source: &Path, frozen: &Path) -> Result<()> {
    let diff = Command::new("git")
        .args([
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-ext-diff",
        ])
        .current_dir(source)
        .output()
        .context("reading staged release changes")?;
    if !diff.status.success() {
        bail!(
            "reading staged release changes failed: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        )
    }
    if diff.stdout.is_empty() {
        return Ok(());
    }
    let mut apply = Command::new("git")
        .args(["apply", "--cached", "--binary"])
        .current_dir(frozen)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("mirroring staged release changes")?;
    apply
        .stdin
        .take()
        .context("opening staged release patch input")?
        .write_all(&diff.stdout)
        .context("writing staged release patch")?;
    let output = apply
        .wait_with_output()
        .context("applying staged release patch")?;
    if !output.status.success() {
        bail!(
            "mirroring staged release changes failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(())
}

fn clear_worktree(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        if entry.file_name().as_os_str() != ".git" {
            remove_path(&entry.path())?;
        }
    }
    Ok(())
}

fn copy_entry(source_root: &Path, destination_root: &Path, entry: &snapshot::Entry) -> Result<()> {
    let source = snapshot_path(source_root, &entry.path)?;
    let destination = snapshot_path(destination_root, &entry.path)?;
    create_parent(destination_root, &destination)?;
    let metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("reading frozen source {}", source.display()))?;
    match entry.kind {
        snapshot::Kind::File if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::copy(&source, &destination).with_context(|| {
                format!(
                    "copying frozen source {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
            fs::set_permissions(&destination, metadata.permissions())
                .with_context(|| format!("preserving mode for {}", destination.display()))?;
        }
        snapshot::Kind::Symlink if metadata.file_type().is_symlink() => {
            let target = fs::read_link(&source)
                .with_context(|| format!("reading symlink {}", source.display()))?;
            copy_symlink(&source, &target, &destination)?;
        }
        snapshot::Kind::Deleted => {}
        _ => bail!("release source changed while materializing {}", entry.path),
    }
    Ok(())
}

fn snapshot_path(root: &Path, stored: &str) -> Result<PathBuf> {
    let relative = Path::new(stored);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid frozen release snapshot path {stored:?}")
    }
    Ok(root.join(relative))
}

fn create_parent(root: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("frozen release entry has no parent")?;
    let relative = parent
        .strip_prefix(root)
        .context("frozen release entry escapes its worktree")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("invalid frozen release parent")
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!(
                "frozen release parent is not a directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .with_context(|| format!("creating {}", current.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", current.display()));
            }
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
    }
}

#[cfg(unix)]
fn copy_symlink(_source: &Path, target: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)
        .with_context(|| format!("creating symlink {}", destination.display()))
}

#[cfg(windows)]
fn copy_symlink(source: &Path, target: &Path, destination: &Path) -> Result<()> {
    if fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
    .with_context(|| format!("creating symlink {}", destination.display()))
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(_source: &Path, _target: &Path, _destination: &Path) -> Result<()> {
    bail!("frozen release does not support symlinks on this platform")
}
