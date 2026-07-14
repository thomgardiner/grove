//! Copy-on-write tree cloning. Because it is copy-on-write (APFS clonefile, ReFS
//! block clone, Linux reflink), seeding a fresh worktree lane from a warm root is
//! near-free and a lane build can never mutate the root it came from.
//!
//! On macOS/Linux this is one whole-tree `cp` clone (a single `clonefile` on APFS),
//! which is far faster than reflinking file by file. On Windows there is no such CLI,
//! so each file is reflinked with `reflink-copy` (ReFS block clone, plain copy on NTFS).

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes concurrent clones' staging directories within one process.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Clone the `src` tree into `dst`, replacing any existing `dst` atomically. The clone
/// lands in a sibling staging directory and is swapped into place only once it fully
/// succeeds, so a failed clone (disk exhaustion, I/O error) never destroys a good
/// existing `dst` — which matters for promote, where `dst` is the shared canonical.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst
        .parent()
        .context("clone destination has no parent directory")?;
    std::fs::create_dir_all(parent)?;

    let tag = format!(
        "{}-{}",
        std::process::id(),
        STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let staging = parent.join(format!(".grove-staging-{tag}"));
    let _ = std::fs::remove_dir_all(&staging);
    if let Err(e) = clone_impl(src, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Publish: move any existing dst aside, swap staging in, then drop the old copy.
    // Restore the original on any failure so dst is never left missing or partial.
    let backup = parent.join(format!(".grove-old-{tag}"));
    if dst.exists() {
        std::fs::rename(dst, &backup).context("moving the old destination aside")?;
    }
    match std::fs::rename(&staging, dst) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(e) => {
            if backup.exists() {
                let _ = std::fs::rename(&backup, dst);
            }
            let _ = std::fs::remove_dir_all(&staging);
            Err(e).context("publishing the cloned tree")
        }
    }
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
/// (`clone_tree` stages into a fresh path). Falls back to a copy on a cross-volume or
/// non-APFS destination, clearing any partial output before each attempt so the copy
/// never nests the source under a half-written destination.
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
    let _ = std::fs::remove_dir_all(dst);
    if cp(&["-cR"], src, dst).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(dst);
    cp(&["-R"], src, dst)
}

/// Linux: `--reflink=auto` reflinks on btrfs/XFS and falls back to a copy elsewhere.
#[cfg(target_os = "linux")]
fn clone_impl(src: &Path, dst: &Path) -> Result<()> {
    cp(&["--reflink=auto", "-R"], src, dst)
}

/// Windows / other: reflink each file (ReFS block clone on a Dev Drive, plain copy on
/// NTFS). A symlink is not silently skipped — that would leave a seed Cargo treats as
/// complete — so seeding a tree that contains one fails loudly instead.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_impl(src: &Path, dst: &Path) -> Result<()> {
    use anyhow::{bail, Context};
    std::fs::create_dir_all(dst)?;
    for entry in walkdir::WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let target = dst.join(rel);
        let ty = entry.file_type();
        if ty.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if ty.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            reflink_copy::reflink_or_copy(entry.path(), &target)
                .with_context(|| format!("reflink {}", entry.path().display()))?;
        } else if ty.is_symlink() {
            bail!(
                "cannot seed a tree containing a symlink on this platform: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}
