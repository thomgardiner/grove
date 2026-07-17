use super::{TEMP_SEQUENCE, git_path};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

pub(crate) struct Objects {
    pub(super) directory: PathBuf,
    pub(super) alternates: OsString,
}

impl Drop for Objects {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

pub(crate) fn temp_objects(worktree: &Path) -> Result<Objects> {
    let real = git_path(worktree, "objects")?;
    let alternates =
        std::env::join_paths([real]).context("encoding the real Git object directory")?;
    for _ in 0..1024 {
        let number = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "grove-salvage-objects-{}-{number}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                return Ok(Objects {
                    directory: path,
                    alternates,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating disposable Git object quarantine"),
        }
    }
    bail!("could not allocate a disposable Git object quarantine")
}
