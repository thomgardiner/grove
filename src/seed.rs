//! Copy-on-write tree cloning. Because it is copy-on-write (APFS clonefile, ReFS
//! block clone, Linux reflink), seeding a fresh worktree lane from a warm root is
//! near-free and a lane build can never mutate the root it came from.
//!
//! On macOS/Linux this is one whole-tree `cp` clone (a single `clonefile` on APFS),
//! which is far faster than reflinking file by file. On Windows there is no such CLI,
//! so each file is reflinked with `reflink-copy` (ReFS block clone, plain copy on NTFS).

use anyhow::Result;
use std::path::Path;

/// Clone the `src` tree into `dst`, replacing any existing `dst`.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    clone_impl(src, dst)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cp(flags: &[&str], src: &Path, dst: &Path) -> Result<()> {
    use anyhow::bail;
    let status = std::process::Command::new("cp")
        .args(flags)
        .arg(src)
        .arg(dst)
        .status()?;
    if !status.success() {
        bail!("cp {flags:?} failed");
    }
    Ok(())
}

/// macOS: `clonefile(2)` clones a directory tree recursively at the APFS metadata
/// level in one syscall — far faster than cloning file by file. `dst` must not exist
/// (`clone_tree` guarantees that). Falls back to a copy on a cross-volume or non-APFS
/// destination.
#[cfg(target_os = "macos")]
fn clone_impl(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src_c = CString::new(src.as_os_str().as_bytes())?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both paths are valid NUL-terminated C strings for the duration of the
    // call; clonefile does not retain them.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    cp(&["-cR"], src, dst).or_else(|_| cp(&["-R"], src, dst))
}

/// Linux: `--reflink=auto` reflinks on btrfs/XFS and falls back to a copy elsewhere.
#[cfg(target_os = "linux")]
fn clone_impl(src: &Path, dst: &Path) -> Result<()> {
    cp(&["--reflink=auto", "-R"], src, dst)
}

/// Windows / other: reflink each file (ReFS block clone on a Dev Drive, plain copy
/// on NTFS). Symlinks inside a build dir are rare and non-load-bearing; skip them.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_impl(src: &Path, dst: &Path) -> Result<()> {
    use anyhow::Context;
    std::fs::create_dir_all(dst)?;
    for entry in walkdir::WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            reflink_copy::reflink_or_copy(entry.path(), &target)
                .with_context(|| format!("reflink {}", entry.path().display()))?;
        }
    }
    Ok(())
}
