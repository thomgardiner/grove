use anyhow::{Context, Result};
use std::collections::hash_map::RandomState;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);
static PROCESS_NONCE: LazyLock<u128> = LazyLock::new(process_nonce);
static PROCESS_START: LazyLock<Option<u64>> = LazyLock::new(|| process_start(std::process::id()));

/// Publish complete bytes at `path` without exposing a partial record.
///
/// The temp file is synchronized before replacement. Unix also synchronizes newly
/// created parent directories and the final directory entry; Windows uses a
/// write-through replacement.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let path = absolute(path)?;
    let parent = path.parent().context("path has no parent directory")?;
    create_parent(parent)?;
    sweep_temps(parent);
    let (temp, file) = create_temp(
        parent,
        std::process::id(),
        *PROCESS_START,
        *PROCESS_NONCE,
        &WRITE_SEQ,
    )?;
    let result = publish(&temp, &path, bytes, file);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn create_temp(
    parent: &Path,
    pid: u32,
    started: Option<u64>,
    nonce: u128,
    sequence: &AtomicU64,
) -> Result<(PathBuf, File)> {
    let started = started
        .map(|value| format!("{value:016x}"))
        .unwrap_or_else(|| "unknown".to_owned());
    for _ in 0..1024 {
        let number = sequence.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".grove-record-{pid}-{started}-{nonce:032x}-{number}.tmp"
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating temp file"),
        }
    }
    anyhow::bail!("could not allocate a unique record temp file")
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn sweep_temps(parent: &Path) {
    for entry in fs::read_dir(parent).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(owner) = temp_owner(&path) else {
            continue;
        };
        if !owner_alive(owner) {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Clone, Copy)]
struct TempOwner {
    pid: u32,
    started: Option<u64>,
    nonce: u128,
}

fn temp_owner(path: &Path) -> Option<TempOwner> {
    let name = path.file_name()?.to_str()?;
    let body = name.strip_prefix(".grove-record-")?.strip_suffix(".tmp")?;
    let mut fields = body.split('-');
    let pid = fields.next()?.parse().ok()?;
    let started = match fields.next()? {
        "unknown" => None,
        value => Some(u64::from_str_radix(value, 16).ok()?),
    };
    let nonce = u128::from_str_radix(fields.next()?, 16).ok()?;
    fields.next()?.parse::<u64>().ok()?;
    (fields.next().is_none()).then_some(TempOwner {
        pid,
        started,
        nonce,
    })
}

fn process_nonce() -> u128 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut hasher = RandomState::new().build_hasher();
    std::process::id().hash(&mut hasher);
    now.hash(&mut hasher);
    (&WRITE_SEQ as *const AtomicU64 as usize).hash(&mut hasher);
    ((now as u128) << 64) | hasher.finish() as u128
}

fn pid_alive(pid: u32) -> bool {
    process_start(pid).is_some()
}

fn process_start(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(sysinfo::Process::start_time)
}

fn owner_alive(owner: TempOwner) -> bool {
    if owner.pid == std::process::id() {
        return owner.nonce == *PROCESS_NONCE;
    }
    match owner.started {
        Some(started) => process_start(owner.pid) == Some(started),
        None => pid_alive(owner.pid),
    }
}

fn publish(temp: &Path, path: &Path, bytes: &[u8], file: File) -> Result<()> {
    publish_after(temp, path, bytes, file, || {})
}

fn publish_after(
    temp: &Path,
    path: &Path,
    bytes: &[u8],
    mut file: File,
    after_sync: impl FnOnce(),
) -> Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    after_sync();
    replace(temp, path)?;
    sync_parent(path.parent().context("path has no parent directory")?)
}

fn create_parent(parent: &Path) -> Result<()> {
    if parent.is_dir() {
        return Ok(());
    }
    let existing = parent
        .ancestors()
        .find(|ancestor| ancestor.is_dir())
        .context("path has no existing ancestor directory")?
        .to_path_buf();
    fs::create_dir_all(parent)?;
    sync_created(parent, &existing)
}

#[cfg(unix)]
fn sync_created(parent: &Path, existing: &Path) -> Result<()> {
    for directory in created_chain(parent, existing) {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn created_chain<'a>(parent: &'a Path, existing: &Path) -> Vec<&'a Path> {
    let mut chain = Vec::new();
    for directory in parent.ancestors() {
        chain.push(directory);
        if directory == existing {
            break;
        }
    }
    chain
}

#[cfg(not(unix))]
fn sync_created(_parent: &Path, _existing: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace(temp: &Path, path: &Path) -> Result<()> {
    fs::rename(temp, path).context("publishing temp file")
}

#[cfg(windows)]
fn replace(temp: &Path, path: &Path) -> Result<()> {
    windows::replace(temp, path)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    File::open(parent)?
        .sync_all()
        .context("syncing record directory")
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result};
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const REPLACE_EXISTING: u32 = 0x1;
    const WRITE_THROUGH: u32 = 0x8;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    pub(super) fn replace(temp: &Path, path: &Path) -> Result<()> {
        let temp = wide(&fs::canonicalize(temp)?)?;
        let parent = fs::canonicalize(path.parent().context("path has no parent directory")?)?;
        let destination = parent.join(path.file_name().context("path has no file name")?);
        let path = wide(&destination)?;
        // SAFETY: both buffers are stable, NUL-terminated UTF-16 paths for this call.
        let moved = unsafe {
            MoveFileExW(
                temp.as_ptr(),
                path.as_ptr(),
                REPLACE_EXISTING | WRITE_THROUGH,
            )
        };
        if moved == 0 {
            return Err(io::Error::last_os_error()).context("publishing temp file");
        }
        Ok(())
    }

    fn wide(path: &Path) -> Result<Vec<u16>> {
        let mut wide: Vec<_> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains NUL",
            ))
            .context("encoding Windows path");
        }
        wide.push(0);
        Ok(wide)
    }
}

#[cfg(test)]
#[path = "cache_atomic_tests.rs"]
mod tests;
