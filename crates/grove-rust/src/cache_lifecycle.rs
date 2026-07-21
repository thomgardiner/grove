use anyhow::{Context, Result, bail};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use super::short_hash;

pub(crate) struct Guard {
    _file: File,
    _local: LocalGuard,
}

#[cfg(unix)]
impl Guard {
    pub(crate) fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;

        self._file.as_raw_fd()
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy)]
enum Admission<'a> {
    Wait,
    Try,
    Until(&'a dyn Fn() -> bool),
}

#[derive(Default)]
struct Local {
    readers: usize,
    writer: bool,
}

#[derive(Default)]
struct State {
    locks: Mutex<HashMap<PathBuf, Local>>,
    ready: Condvar,
}

struct LocalGuard {
    path: PathBuf,
    mode: Mode,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(State::default)
}

fn local(path: &Path, mode: Mode, admission: Admission<'_>) -> Option<LocalGuard> {
    let state = state();
    let mut locks = state
        .locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let lock = locks.entry(path.to_path_buf()).or_default();
        let available = match mode {
            Mode::Shared => !lock.writer,
            Mode::Exclusive => !lock.writer && lock.readers == 0,
        };
        if available {
            match mode {
                Mode::Shared => lock.readers += 1,
                Mode::Exclusive => lock.writer = true,
            }
            return Some(LocalGuard {
                path: path.to_path_buf(),
                mode,
            });
        }
        match admission {
            Admission::Wait => {
                locks = state
                    .ready
                    .wait(locks)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Admission::Try => return None,
            Admission::Until(cancelled) => {
                if cancelled() {
                    return None;
                }
                locks = state
                    .ready
                    .wait_timeout(locks, Duration::from_millis(25))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        }
    }
}

impl Drop for LocalGuard {
    fn drop(&mut self) {
        let state = state();
        let mut locks = state
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = if let Some(lock) = locks.get_mut(&self.path) {
            match self.mode {
                Mode::Shared => lock.readers = lock.readers.saturating_sub(1),
                Mode::Exclusive => lock.writer = false,
            }
            !lock.writer && lock.readers == 0
        } else {
            false
        };
        if remove {
            locks.remove(&self.path);
        }
        state.ready.notify_all();
    }
}

fn identity(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path)
            .with_context(|| format!("canonicalizing lifecycle workspace {}", path.display()));
    }
    let parent = path
        .parent()
        .context("planned lifecycle workspace has no parent")?;
    let name = path
        .file_name()
        .context("planned lifecycle workspace has no name")?;
    Ok(fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing lifecycle parent {}", parent.display()))?
        .join(name))
}

fn path(root: &Path, repo: &str, workspace: &Path) -> Result<PathBuf> {
    let workspace = identity(workspace)?;
    let workspace = workspace.to_string_lossy();
    Ok(root.join("locks").join(format!(
        "lifecycle-{}.lock",
        short_hash(&[repo, &workspace])
    )))
}

fn open(root: &Path, repo: &str, workspace: &Path) -> Result<(PathBuf, File)> {
    let path = path(root, repo, workspace)?;
    fs::create_dir_all(path.parent().context("lifecycle lock has no parent")?)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening lifecycle lock {}", path.display()))?;
    Ok((path, file))
}

fn file_lock(file: &File, mode: Mode, admission: Admission<'_>) -> Result<bool> {
    let blocking = || match mode {
        Mode::Shared => FileExt::lock_shared(file),
        Mode::Exclusive => FileExt::lock_exclusive(file),
    };
    let probing = || match mode {
        Mode::Shared => FileExt::try_lock_shared(file),
        Mode::Exclusive => FileExt::try_lock_exclusive(file),
    };
    match admission {
        Admission::Wait => {
            blocking()?;
            Ok(true)
        }
        Admission::Try => match probing() {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error.into()),
        },
        Admission::Until(cancelled) => loop {
            if cancelled() {
                return Ok(false);
            }
            match probing() {
                Ok(()) => return Ok(true),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        },
    }
}

fn lock(
    root: &Path,
    repo: &str,
    workspace: &Path,
    mode: Mode,
    admission: Admission<'_>,
) -> Result<Option<Guard>> {
    let (path, file) = open(root, repo, workspace)?;
    let Some(local) = local(&path, mode, admission) else {
        return Ok(None);
    };
    if !file_lock(&file, mode, admission)
        .with_context(|| format!("locking lifecycle gate {}", path.display()))?
    {
        return Ok(None);
    }
    Ok(Some(Guard {
        _file: file,
        _local: local,
    }))
}

pub(crate) fn shared(root: &Path, workspace: &Path) -> Result<Guard> {
    shared_with(root, workspace, Admission::Wait)?
        .context("blocking lifecycle admission returned busy")
}

pub(crate) fn try_shared(root: &Path, workspace: &Path) -> Result<Option<Guard>> {
    shared_with(root, workspace, Admission::Try)
}

pub(crate) fn shared_until(
    root: &Path,
    workspace: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Guard>> {
    shared_with(root, workspace, Admission::Until(cancelled))
}

fn shared_with(root: &Path, workspace: &Path, admission: Admission<'_>) -> Result<Option<Guard>> {
    let workspace = fs::canonicalize(workspace)
        .with_context(|| format!("workspace {} is not available", workspace.display()))?;
    let repo = crate::project::repo_identity(&workspace);
    let Some(guard) = lock(root, &repo, &workspace, Mode::Shared, admission)? else {
        return Ok(None);
    };
    let current = fs::canonicalize(&workspace)
        .with_context(|| format!("workspace {} disappeared", workspace.display()))?;
    if current != workspace || crate::project::repo_identity(&current) != repo {
        bail!("workspace identity changed while acquiring its lifecycle gate")
    }
    Ok(Some(guard))
}

pub(crate) fn exclusive(root: &Path, repo: &str, workspace: &Path) -> Result<Guard> {
    lock(root, repo, workspace, Mode::Exclusive, Admission::Wait)?
        .context("blocking lifecycle admission returned busy")
}

pub(crate) fn try_exclusive(root: &Path, repo: &str, workspace: &Path) -> Result<Option<Guard>> {
    lock(root, repo, workspace, Mode::Exclusive, Admission::Try).context("probing lifecycle gate")
}

#[cfg(test)]
#[path = "cache_lifecycle_tests.rs"]
mod tests;
