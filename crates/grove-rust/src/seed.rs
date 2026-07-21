//! Copy-on-write tree cloning. Because it is copy-on-write (APFS clonefile, ReFS
//! block clone, Linux reflink), seeding a fresh worktree lane from a warm root is
//! near-free and a lane build can never mutate the root it came from.
//!
//! On macOS/Linux this is one whole-tree `cp` clone (a single `clonefile` on APFS),
//! which is far faster than reflinking file by file. On Windows there is no such CLI,
//! so each file is reflinked with `reflink-copy` (ReFS block clone, plain copy on NTFS).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The result of a strict copy-on-write probe for one cache filesystem.
#[derive(Serialize)]
pub struct CowProbe {
    pub cache_root: PathBuf,
    pub status: CowProbeStatus,
    pub platform: &'static str,
    pub method: &'static str,
    pub semantics: &'static str,
    pub detail: Option<String>,
}

/// Whether a strict clone could be completed without a byte-copy fallback.
#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CowProbeStatus {
    Supported,
    Unavailable,
    Error,
}

/// Distinguishes concurrent clones' staging directories within one process.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);
static PROCESS_START: LazyLock<Option<u64>> = LazyLock::new(|| process_start(std::process::id()));

#[derive(Serialize, Deserialize)]
struct ScratchOwner {
    schema: u32,
    name: String,
    pid: u32,
    started: u64,
}

/// Clone the `src` tree into `dst`, replacing any existing `dst` atomically. The clone
/// lands in a sibling staging directory and is swapped into place only once it fully
/// succeeds, so a failed clone (disk exhaustion, I/O error) never destroys a good
/// existing `dst` — which matters for promote, where `dst` is the shared canonical.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    clone_tree_cow(src, dst, false)
}

/// Like [`clone_tree`], but when `require_cow` is set it fails instead of falling back to a
/// full byte copy. On a filesystem without copy-on-write (NTFS, a non-APFS or non-reflink
/// volume) a "seed" would be a full copy of a multi-gigabyte target dir — slower and more
/// disk than just building cold — so a caller that only wants the CoW win can refuse it.
pub fn clone_tree_cow(src: &Path, dst: &Path, require_cow: bool) -> Result<()> {
    clone_tree_cow_with(src, dst, require_cow, |_| Ok(()))
}

/// Probe strict copy-on-write support using only owned, temporary paths under `root`.
/// A successful result proves this process can make a strict clone there; it does not
/// claim that every future clone will succeed (for example, disk exhaustion can change).
pub fn probe_cow(root: &Path) -> CowProbe {
    let mut probe = CowProbe {
        cache_root: root.to_path_buf(),
        status: CowProbeStatus::Error,
        platform: std::env::consts::OS,
        method: cow_method(),
        semantics: "supported means the strict clone path completed without a byte-copy fallback",
        detail: None,
    };
    if let Err(error) = std::fs::create_dir_all(root) {
        probe.detail = Some(format!("creating cache root: {error}"));
        return probe;
    }
    let src = match reserve(root, "cow-probe-src") {
        Ok(path) => path,
        Err(error) => {
            probe.detail = Some(format!("reserving source probe path: {error}"));
            return probe;
        }
    };
    let dst = match reserve(root, "cow-probe-dst") {
        Ok(path) => path,
        Err(error) => {
            clear(&src);
            probe.detail = Some(format!("reserving destination probe path: {error}"));
            return probe;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&src)
        .and_then(|()| std::fs::write(src.join("probe"), b"grove-cow-probe"))
    {
        clear(&src);
        clear(&dst);
        probe.detail = Some(format!("writing probe source: {error}"));
        return probe;
    }
    let result = clone_tree_cow(&src, &dst, true);
    let cleaned = clear(&src) && clear(&dst);
    if !cleaned {
        probe.detail = Some("cleaning owned probe paths".into());
    } else if let Err(error) = result {
        probe.status = CowProbeStatus::Unavailable;
        probe.detail = Some(error.to_string());
    } else {
        probe.status = CowProbeStatus::Supported;
    }
    probe
}

#[cfg(target_os = "macos")]
fn cow_method() -> &'static str {
    "clonefile(2)"
}

#[cfg(target_os = "linux")]
fn cow_method() -> &'static str {
    "cp --reflink=always"
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn cow_method() -> &'static str {
    "strict per-file reflink"
}

/// Clone a tree and transform its private staging copy before publishing it. A failed
/// transform leaves the existing destination untouched.
pub(crate) fn clone_tree_cow_with(
    src: &Path,
    dst: &Path,
    require_cow: bool,
    transform: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let parent = dst
        .parent()
        .context("clone destination has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let staging = reserve(parent, "staging")?;
    if let Err(e) = clone_impl(src, &staging, require_cow) {
        clear(&staging);
        return Err(e);
    }
    if let Err(e) = transform(&staging) {
        clear(&staging);
        return Err(e);
    }
    publish(&staging, dst, parent)
}

fn publish(staging: &Path, dst: &Path, parent: &Path) -> Result<()> {
    // Publish: move any existing dst aside, swap staging in, then drop the old copy.
    // Restore the original on any failure so dst is never left missing or partial.
    let backup = if dst.exists() {
        let backup = reserve(parent, "old").inspect_err(|_| {
            clear(staging);
        })?;
        if let Err(error) = std::fs::rename(dst, &backup) {
            clear(&backup);
            clear(staging);
            return Err(error).context("moving the old destination aside");
        }
        Some(backup)
    } else {
        None
    };
    match std::fs::rename(staging, dst) {
        Ok(()) => {
            clear(staging);
            if let Some(backup) = backup {
                clear(&backup);
            }
            Ok(())
        }
        Err(e) => {
            if let Some(backup) = backup
                && std::fs::rename(&backup, dst).is_ok()
            {
                clear(&backup);
            }
            clear(staging);
            Err(e).context("publishing the cloned tree")
        }
    }
}

fn reserve(parent: &Path, kind: &str) -> Result<PathBuf> {
    let pid = std::process::id();
    let started = (*PROCESS_START).context("reading clone process identity")?;
    for _ in 0..1024 {
        let sequence = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!(".grove-{kind}-{pid}-{started:016x}-{sequence}");
        let path = parent.join(&name);
        if path.exists() {
            continue;
        }
        let owner = ScratchOwner {
            schema: 1,
            name,
            pid,
            started,
        };
        if record(&path, &owner)? {
            return Ok(path);
        }
    }
    anyhow::bail!("could not allocate clone scratch identity")
}

fn record(path: &Path, owner: &ScratchOwner) -> Result<bool> {
    // ponytail: a crash can leave one tiny orphan sidecar; sweep files if observed.
    let sidecar = sidecar(path);
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sidecar)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error).context("creating clone scratch identity"),
    };
    let result = serde_json::to_writer(&mut file, owner)
        .context("writing clone scratch identity")
        .and_then(|()| file.sync_all().context("syncing clone scratch identity"))
        .and_then(|()| sync(path.parent().context("clone scratch has no parent")?));
    if result.is_err() || path.exists() {
        let _ = std::fs::remove_file(&sidecar);
    }
    result.map(|()| !path.exists())
}

fn sidecar(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.owner.json"))
}

fn clear(path: &Path) -> bool {
    let removed = std::fs::remove_dir_all(path).is_ok() || !path.exists();
    if removed {
        let _ = std::fs::remove_file(sidecar(path));
    }
    removed
}

fn abandoned(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let owner = std::fs::read(sidecar(path))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ScratchOwner>(&bytes).ok());
    owner.is_some_and(|owner| {
        owner.schema == 1 && owner.name == name && process_start(owner.pid) != Some(owner.started)
    })
}

pub(crate) fn reap(path: &Path) -> bool {
    abandoned(path) && clear(path)
}

fn process_start(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(sysinfo::Process::start_time)
}

#[cfg(unix)]
fn sync(parent: &Path) -> Result<()> {
    File::open(parent)?
        .sync_all()
        .context("syncing clone parent")
}

#[cfg(not(unix))]
fn sync(_parent: &Path) -> Result<()> {
    Ok(())
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
/// (`clone_tree` stages into a fresh path). Strict CoW accepts only `clonefile`: macOS
/// `cp -c` may silently fall back to a byte copy. Partial output is cleared before each
/// attempt so a copy never nests the source under a half-written destination.
#[cfg(target_os = "macos")]
fn clone_impl(src: &Path, dst: &Path, require_cow: bool) -> Result<()> {
    use anyhow::bail;
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
    if require_cow {
        bail!(
            "copy-on-write clone unavailable for {} (not an APFS volume?); refusing a full copy",
            dst.display()
        );
    }
    if cp(&["-cR"], src, dst).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(dst);
    cp(&["-R"], src, dst)
}

/// Linux: `--reflink=always` reflinks or fails; `--reflink=auto` reflinks on btrfs/XFS
/// and falls back to a copy elsewhere.
#[cfg(target_os = "linux")]
fn clone_impl(src: &Path, dst: &Path, require_cow: bool) -> Result<()> {
    let reflink = if require_cow {
        "--reflink=always"
    } else {
        "--reflink=auto"
    };
    cp(&[reflink, "-R"], src, dst)
}

/// Windows / other: reflink each file (ReFS block clone on a Dev Drive, plain copy on
/// NTFS). With `require_cow`, use a strict reflink that fails on NTFS instead of silently
/// copying. A symlink is not silently skipped — that would leave a seed Cargo treats as
/// complete — so seeding a tree that contains one fails loudly instead.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_impl(src: &Path, dst: &Path, require_cow: bool) -> Result<()> {
    use anyhow::{Context, bail};
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
            if require_cow {
                reflink_copy::reflink(entry.path(), &target)
                    .with_context(|| format!("copy-on-write reflink {}", entry.path().display()))?;
            } else {
                reflink_copy::reflink_or_copy(entry.path(), &target)
                    .with_context(|| format!("reflink {}", entry.path().display()))?;
            }
        } else if ty.is_symlink() {
            bail!(
                "cannot seed a tree containing a symlink on this platform: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CowProbeStatus, probe_cow};

    #[test]
    fn cow_probe_cleans_its_owned_paths() {
        let root = tempfile::tempdir().expect("temporary cache root");
        let probe = probe_cow(root.path());
        assert_ne!(probe.status, CowProbeStatus::Error);
        let leftovers = std::fs::read_dir(root.path())
            .expect("read cache root")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".grove-cow-probe")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn cow_probe_reports_setup_errors() {
        let root = tempfile::tempdir().expect("temporary parent");
        let file = root.path().join("cache-root-file");
        std::fs::write(&file, b"not a directory").expect("cache-root file");
        let probe = probe_cow(&file);
        assert_eq!(probe.status, CowProbeStatus::Error);
        assert!(probe.detail.is_some());
    }
}
