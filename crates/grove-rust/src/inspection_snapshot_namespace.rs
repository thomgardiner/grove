use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(super) fn root(path: &Path, source: &Path, create: bool) -> Result<PathBuf> {
    reject_ancestors(path)?;
    let intended = future(path)?;
    if intended.starts_with(source) {
        bail!("inspection capsules must live outside the source workspace")
    }
    directory(&intended, create)
}

pub(super) fn child(parent: &Path, name: &str, create: bool) -> Result<PathBuf> {
    let child = directory(&parent.join(name), create)?;
    if !child.starts_with(parent) {
        bail!("inspection namespace escaped its canonical state root")
    }
    Ok(child)
}

fn directory(path: &Path, create: bool) -> Result<PathBuf> {
    if create {
        ensure(path)
    } else {
        let metadata = fs::symlink_metadata(path).context("reading inspection namespace")?;
        if !real_dir(&metadata) {
            bail!("inspection namespace contains a redirect or non-directory")
        }
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))
    }
}

fn reject_ancestors(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestors: Vec<_> = absolute.ancestors().collect();
    ancestors.reverse();
    for ancestor in ancestors {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if real_dir(&metadata) => {}
            Ok(_) => bail!("inspection state path has a redirecting ancestor"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error).context("reading inspection state path"),
        }
    }
    Ok(())
}

fn ensure(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if real_dir(&metadata) => {}
        Ok(_) => bail!("inspection namespace contains a redirect or non-directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create(path)?,
        Err(error) => return Err(error).context("reading inspection namespace"),
    }
    fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))
}

fn create(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("inspection namespace has no parent")?;
    let parent = ensure(parent)?;
    let name = path
        .file_name()
        .context("inspection namespace has no directory name")?;
    let child = parent.join(name);
    match fs::create_dir(&child) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("creating inspection namespace"),
    }
    if !real_dir(&fs::symlink_metadata(&child)?) {
        bail!("inspection namespace creation encountered a redirect")
    }
    Ok(())
}

fn future(path: &Path) -> Result<PathBuf> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("inspection state path must not contain '..'")
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    let mut current = absolute.as_path();
    loop {
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                if !real_dir(&fs::symlink_metadata(current)?) {
                    bail!("inspection state path has a redirecting ancestor")
                }
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    current
                        .file_name()
                        .context("inspection state path is invalid")?,
                );
                current = current
                    .parent()
                    .context("inspection state path has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("resolving inspection state path"),
        }
    }
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
