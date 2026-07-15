//! Held directory identities for frozen release worktrees.

use anyhow::{Context, Result, bail};
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) struct PinnedDirectory {
    pub(super) path: PathBuf,
    #[cfg(unix)]
    pub(super) file: File,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(not(unix))]
    resolved: PathBuf,
}

#[cfg(unix)]
impl PinnedDirectory {
    pub(super) fn open(path: &Path, what: &str) -> Result<Self> {
        let expected = fs::symlink_metadata(path).with_context(|| format!("reading {what}"))?;
        if !expected.is_dir() || expected.file_type().is_symlink() {
            bail!("{what} is not a real directory")
        }
        let file = File::open(path).with_context(|| format!("opening {what}"))?;
        let actual = file.metadata().with_context(|| format!("reading {what}"))?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            bail!("{what} changed while opening it")
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            dev: actual.dev(),
            ino: actual.ino(),
        })
    }

    pub(super) fn open_at(parent: &Self, name: &Path, path: PathBuf, what: &str) -> Result<Self> {
        use rustix::fs::{Mode, OFlags, openat};

        let file = File::from(
            openat(
                &parent.file,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| format!("opening {what}"))?,
        );
        let metadata = file.metadata().with_context(|| format!("reading {what}"))?;
        Ok(Self {
            path,
            file,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    pub(super) fn matches(&self, what: &str) -> Result<()> {
        self.matches_path(&self.path, what)
    }

    pub(super) fn matches_path(&self, path: &Path, what: &str) -> Result<()> {
        let metadata = fs::symlink_metadata(path).with_context(|| format!("reading {what}"))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.dev() != self.dev
            || metadata.ino() != self.ino
        {
            bail!("{what} changed while frozen release was active")
        }
        Ok(())
    }
}

#[cfg(not(unix))]
impl PinnedDirectory {
    pub(super) fn open(path: &Path, what: &str) -> Result<Self> {
        let metadata = fs::symlink_metadata(path).with_context(|| format!("reading {what}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("{what} is not a real directory")
        }
        Ok(Self {
            path: path.to_path_buf(),
            resolved: fs::canonicalize(path).with_context(|| format!("resolving {what}"))?,
        })
    }

    pub(super) fn matches(&self, what: &str) -> Result<()> {
        self.matches_path(&self.path, what)
    }

    pub(super) fn matches_path(&self, path: &Path, what: &str) -> Result<()> {
        if fs::canonicalize(path).with_context(|| format!("resolving {what}"))? != self.resolved {
            bail!("{what} changed while frozen release was active")
        }
        Ok(())
    }
}
