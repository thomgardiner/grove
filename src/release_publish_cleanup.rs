//! Descriptor-rooted cleanup for abandoned frozen-release directories.

#[cfg(unix)]
use anyhow::{Context, Result};
#[cfg(unix)]
use std::{fs::File, path::Path};

/// Remove only entries reached from the supplied directory descriptor.
#[cfg(unix)]
pub(super) fn clear<Fd: std::os::fd::AsFd>(directory: Fd) -> Result<()> {
    clear_except(directory, None)
}

/// Clear a worktree descriptor without touching its Git administrative file.
#[cfg(unix)]
pub(super) fn clear_except_git<Fd: std::os::fd::AsFd>(directory: Fd) -> Result<()> {
    clear_except(directory, Some(b".git"))
}

#[cfg(unix)]
fn clear_except<Fd: std::os::fd::AsFd>(directory: Fd, skip: Option<&[u8]>) -> Result<()> {
    use rustix::fs::{AtFlags, Dir, Mode, OFlags, openat, unlinkat};
    use rustix::io::Errno;

    let mut entries = Dir::read_from(&directory).context("reading frozen release directory")?;
    while let Some(entry) = entries.read() {
        let entry = entry.context("reading frozen release entry")?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..")
            || skip.is_some_and(|skip| name.to_bytes() == skip)
        {
            continue;
        }
        match openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(child) => {
                clear(&child)?;
                unlinkat(&directory, name, AtFlags::REMOVEDIR)
                    .context("removing frozen release directory")?;
            }
            Err(Errno::NOTDIR | Errno::LOOP) => {
                unlinkat(&directory, name, AtFlags::empty())
                    .context("removing frozen release entry")?;
            }
            Err(error) => return Err(error).context("opening frozen release entry"),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn remove_empty(parent: &File, name: &Path) {
    let _ = rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::REMOVEDIR);
}
