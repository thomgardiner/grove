//! Best available portable write denial for capsule bytes.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub(super) fn seal(root: &Path) -> Result<()> {
    permissions(root, false)
}

pub(super) fn unseal(root: &Path) -> Result<()> {
    permissions(root, true)
}

fn permissions(root: &Path, writable: bool) -> Result<()> {
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if writable {
        entries.reverse();
    }
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        set(path, metadata.permissions(), writable)
            .with_context(|| format!("setting inspection permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set(path: &Path, permissions: fs::Permissions, writable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = permissions.mode();
    let mode = if writable {
        mode | 0o200
    } else {
        mode & !0o222
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set(path: &Path, mut permissions: fs::Permissions, writable: bool) -> Result<()> {
    permissions.set_readonly(!writable);
    fs::set_permissions(path, permissions)?;
    Ok(())
}
