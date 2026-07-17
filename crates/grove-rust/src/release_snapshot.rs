//! Isolated worktree materialization for frozen release verification.

use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, snapshot};

#[path = "release_snapshot_pin.rs"]
mod pin;

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    path: PathBuf,
    #[cfg(unix)]
    base: pin::PinnedDirectory,
}

/// A detached Git worktree whose visible files exactly match the captured snapshot.
/// Dropping it removes only this worktree and its one-use release lane can then be
/// discarded independently.
pub(super) struct FrozenWorkspace {
    source: PathBuf,
    path: PathBuf,
    pin: pin::PinnedDirectory,
    cleaned: bool,
}

impl FrozenWorkspace {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.pin.matches("frozen release worktree")?;
        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .current_dir(&self.source)
            .output()
            .context("removing frozen release worktree")?;
        if !output.status.success() {
            bail!(
                "removing frozen release worktree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        if self.path.exists() {
            self.pin.matches("frozen release worktree")?;
            #[cfg(unix)]
            super::cleanup::clear(&self.pin.file)?;
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for FrozenWorkspace {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Start from `HEAD`, restore the captured index tree, then replace all visible files
/// with the captured bytes. The final capture proves staged, dirty, untracked, and
/// deleted paths survived the transfer exactly without re-reading a mutable source index.
pub(super) fn materialize(
    root: &Path,
    source: &Path,
    start: &snapshot::Snapshot,
) -> Result<FrozenWorkspace> {
    #[cfg(not(unix))]
    {
        let _ = (root, source, start);
        bail!("secure frozen-release materialization is not supported on this platform")
    }
    #[cfg(unix)]
    {
        snapshot::validate_frozen_links(source, start)?;
        let scratch = scratch_path(root, source)?;
        let head = start.head()?;
        let output = Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&scratch.path)
            .arg(head)
            .current_dir(source)
            .output()
            .context("creating frozen release worktree")?;
        if !output.status.success() {
            bail!(
                "creating frozen release worktree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        let pin = pin::PinnedDirectory::open_at(
            &scratch.base,
            Path::new(
                scratch
                    .path
                    .file_name()
                    .context("frozen release worktree needs a directory name")?,
            ),
            scratch.path.clone(),
            "frozen release worktree",
        )?;
        let frozen = FrozenWorkspace {
            source: source.to_path_buf(),
            path: scratch.path,
            pin,
            cleaned: false,
        };
        restore_index(source, &frozen.pin.file, start)?;
        clear_worktree(&frozen.pin.file)?;
        for entry in &start.entries {
            if entry.kind != snapshot::Kind::Deleted {
                copy_entry(source, &frozen.pin.file, entry)?;
            }
        }
        frozen.pin.matches("frozen release worktree")?;
        if snapshot::capture(frozen.path())? != *start {
            bail!("could not materialize the captured release snapshot")
        }
        Ok(frozen)
    }
}

fn scratch_path(root: &Path, source: &Path) -> Result<Scratch> {
    let source = cache::canonical_path(source);
    let root = cache::canonical_path(root);
    let base = if root.starts_with(&source) {
        std::env::temp_dir().join("grove-release-worktrees")
    } else {
        root.join("release-worktrees")
    }
    .join(cache::repo_slug(&source.to_string_lossy()));
    fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let base = pin::PinnedDirectory::open(&base, "release worktree base")?;
    let resolved = fs::canonicalize(&base.path).context("resolving release worktree base")?;
    if resolved.starts_with(&source) {
        bail!("release worktree base must be outside the workspace")
    }
    base.matches_path(&resolved, "release worktree base")?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for _ in 0..64 {
        let sequence = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base.path.join(format!("freeze-{nanos:x}-{sequence:x}"));
        if !path.exists() {
            return Ok(Scratch {
                path,
                #[cfg(unix)]
                base,
            });
        }
    }
    bail!("could not allocate a frozen release worktree path")
}

#[cfg(unix)]
fn restore_index(source: &Path, frozen: &File, snapshot: &snapshot::Snapshot) -> Result<()> {
    use rustix::fs::{Mode, OFlags, openat};

    let tree = snapshot.index_tree()?;
    let mut file = File::from(
        openat(
            frozen,
            Path::new(".git"),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("opening frozen release Git directory link")?,
    );
    let mut link = String::new();
    file.read_to_string(&mut link)
        .context("reading frozen release Git directory link")?;
    let gitdir = gitdir(source, &link)?;
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .arg("--git-dir")
        .arg(gitdir)
        .args(["read-tree", tree])
        .output()
        .context("restoring frozen release index")?;
    if !output.status.success() {
        bail!(
            "restoring frozen release index failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(())
}

#[cfg(unix)]
fn gitdir(source: &Path, link: &str) -> Result<PathBuf> {
    let link = link
        .strip_prefix("gitdir: ")
        .context("frozen release Git directory link is malformed")?
        .trim_end_matches(['\r', '\n']);
    if link.is_empty() || link.contains(['\0', '\n', '\r']) || !Path::new(link).is_absolute() {
        bail!("frozen release Git directory link is not an absolute path")
    }
    let configured = Path::new(link);
    let metadata =
        fs::symlink_metadata(configured).context("reading frozen release Git directory")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("frozen release Git directory is not a real directory")
    }
    let gitdir = fs::canonicalize(configured).context("resolving frozen release Git directory")?;
    let common = common_git_dir(source)?;
    let worktrees = fs::canonicalize(common.join("worktrees"))
        .context("resolving source Git worktree directory")?;
    if gitdir.parent() != Some(worktrees.as_path()) {
        bail!("frozen release Git directory is outside the source worktree registry")
    }
    Ok(gitdir)
}

#[cfg(unix)]
fn common_git_dir(source: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(source)
        .output()
        .context("locating source Git directory")?;
    if !output.status.success() {
        bail!(
            "locating source Git directory failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let path = String::from_utf8(output.stdout)
        .context("source Git directory is not UTF-8")?
        .trim()
        .to_string();
    let path = Path::new(&path);
    fs::canonicalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        source.join(path)
    })
    .context("resolving source Git directory")
}

#[cfg(unix)]
fn clear_worktree(root: &File) -> Result<()> {
    super::cleanup::clear_except_git(root)
}

#[cfg(unix)]
fn copy_entry(source_root: &Path, destination_root: &File, entry: &snapshot::Entry) -> Result<()> {
    use rustix::fs::{Mode, OFlags, openat, symlinkat};

    let relative = entry_path(&entry.path)?;
    let source = source_root.join(&relative);
    let metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("reading frozen source {}", source.display()))?;
    let parent = super::directory::parent(destination_root, &relative, true, "frozen release")?;
    let name = relative
        .file_name()
        .context("frozen release entry has no file name")?;
    match entry.kind {
        snapshot::Kind::File if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let mut input = File::open(&source)
                .with_context(|| format!("opening frozen source {}", source.display()))?;
            if !input.metadata()?.is_file() {
                bail!("release source changed while materializing {}", entry.path)
            }
            let mut output = File::from(
                openat(
                    &parent,
                    name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .with_context(|| format!("creating frozen entry {}", entry.path))?,
            );
            std::io::copy(&mut input, &mut output)
                .with_context(|| format!("copying frozen source {}", source.display()))?;
            output.sync_all()?;
            output.set_permissions(metadata.permissions())?;
        }
        snapshot::Kind::Symlink if metadata.file_type().is_symlink() => {
            let target = fs::read_link(&source)
                .with_context(|| format!("reading symlink {}", source.display()))?;
            symlinkat(&target, &parent, name)
                .with_context(|| format!("creating frozen symlink {}", entry.path))?;
        }
        snapshot::Kind::Deleted => {}
        _ => bail!("release source changed while materializing {}", entry.path),
    }
    Ok(())
}

#[cfg(unix)]
fn entry_path(stored: &str) -> Result<PathBuf> {
    let relative = Path::new(stored);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid frozen release snapshot path {stored:?}")
    }
    Ok(relative.to_path_buf())
}

#[cfg(all(test, unix))]
#[path = "release_snapshot_tests.rs"]
mod tests;
