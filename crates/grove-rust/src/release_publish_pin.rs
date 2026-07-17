//! Held directory identities for frozen-release staging.

#![cfg(unix)]

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cache;

static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

pub(super) fn stage_base(root: &Path, workspace: &Path) -> Result<PinnedDirectory> {
    let workspace = cache::canonical_path(workspace);
    let root = cache::canonical_path(root);
    let base = if root.starts_with(&workspace) {
        std::env::temp_dir().join("grove-release-staging")
    } else {
        root.join("release-staging")
    };
    fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let base = PinnedDirectory::open(&base, "release staging base")?;
    let path = fs::canonicalize(&base.path).context("resolving release staging base")?;
    if path.starts_with(&workspace) {
        bail!("release staging base must be outside the workspace")
    }
    base.matches_path(&path, "release staging base")?;
    Ok(base)
}

pub(super) fn create_stage(base: &PinnedDirectory) -> Result<(PathBuf, PinnedDirectory)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for _ in 0..64 {
        let name = OsString::from(format!(
            "freeze-{nanos:x}-{:x}",
            STAGE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path = base.path.join(&name);
        match rustix::fs::mkdirat(&base.file, Path::new(&name), rustix::fs::Mode::RWXU) {
            Ok(()) => {
                return Ok((
                    path.clone(),
                    PinnedDirectory::open_at(base, Path::new(&name), path, "release stage")?,
                ));
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    bail!("could not allocate a frozen release stage")
}

pub(super) struct PinnedDirectory {
    pub(super) path: PathBuf,
    pub(super) file: File,
    dev: u64,
    ino: u64,
}

impl PinnedDirectory {
    pub(super) fn matches(&self, what: &str) -> Result<()> {
        self.matches_path(&self.path, what)
    }

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

    fn open_at(parent: &Self, name: &Path, path: PathBuf, what: &str) -> Result<Self> {
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
