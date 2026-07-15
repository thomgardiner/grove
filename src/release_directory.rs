//! Descriptor-rooted directory traversal for frozen release writes.

#[cfg(unix)]
use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::path::{Component, Path};

/// Open the parent of `path` below `root`, optionally creating missing directories.
#[cfg(unix)]
pub(super) fn parent(root: &File, path: &Path, create: bool, what: &str) -> Result<File> {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};
    use rustix::io::Errno;

    let mut current = root
        .try_clone()
        .with_context(|| format!("cloning {what} directory"))?;
    for component in path.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(component) = component else {
            bail!("{what} path contains a non-normal component")
        };
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let child = match openat(&current, component, flags, Mode::empty()) {
            Ok(child) => child,
            Err(Errno::NOENT) if create => {
                match mkdirat(&current, component, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => {
                        return Err(error).with_context(|| format!("creating {what} parent"));
                    }
                }
                openat(&current, component, flags, Mode::empty())
                    .with_context(|| format!("opening {what} parent"))?
            }
            Err(error) => return Err(error).with_context(|| format!("opening {what} parent")),
        };
        current = File::from(child);
    }
    Ok(current)
}
