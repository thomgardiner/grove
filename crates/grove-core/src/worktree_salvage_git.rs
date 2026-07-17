use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const INDEX_LOCK_MARKER: &[u8] = b"grove salvage index transaction\n";

#[path = "worktree_salvage_objects.rs"]
mod objects;
pub(super) use objects::{Objects, temp_objects};

#[derive(PartialEq, Eq)]
pub(super) struct Sparse {
    pub(super) patterns: Option<Vec<u8>>,
    pub(super) config: Vec<u8>,
}

pub(super) struct TempIndex(PathBuf);

struct IndexLock(PathBuf);

impl TempIndex {
    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn sparse(worktree: &Path) -> Result<Sparse> {
    let path = git_path(worktree, "info/sparse-checkout")?;
    let patterns = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let config = optional_bytes(
        worktree,
        &[
            "config",
            "--null",
            "--get-regexp",
            "^(core\\.sparseCheckout|core\\.sparseCheckoutCone|index\\.sparse)$",
        ],
    )?
    .unwrap_or_default();
    Ok(Sparse { patterns, config })
}

pub(super) fn index_path(worktree: &Path) -> Result<PathBuf> {
    git_path(worktree, "index")
}

pub(super) fn recover_index_lock(worktree: &Path, has_archive: bool) -> Result<()> {
    let Some(lock) = recoverable_index_lock(worktree, has_archive)? else {
        return Ok(());
    };
    fs::remove_file(&lock)
        .with_context(|| format!("recovering interrupted salvage lock {}", lock.display()))
}

pub(super) fn check_index_lock(worktree: &Path, has_archive: bool) -> Result<()> {
    recoverable_index_lock(worktree, has_archive).map(drop)
}

fn recoverable_index_lock(worktree: &Path, has_archive: bool) -> Result<Option<PathBuf>> {
    let lock = index_path(worktree)?.with_file_name("index.lock");
    let contents = match fs::read(&lock) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", lock.display())),
    };
    if contents != INDEX_LOCK_MARKER || !has_archive {
        bail!(
            "Git index is locked at {}; worktree left intact",
            lock.display()
        )
    }
    Ok(Some(lock))
}

fn git_path(worktree: &Path, name: &str) -> Result<PathBuf> {
    let path = text(worktree, &["rev-parse", "--git-path", name], None)?;
    let path = PathBuf::from(path);
    Ok(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    })
}

pub(super) fn temp_index(index: &Path, contents: &[u8]) -> Result<TempIndex> {
    let parent = index
        .parent()
        .context("Git index has no parent directory")?;
    for _ in 0..1024 {
        let number = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".grove-salvage-{}-{number}.index",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(contents)?;
                file.sync_all()?;
                return Ok(TempIndex(path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating disposable salvage index"),
        }
    }
    bail!("could not allocate a disposable salvage index")
}

pub(super) fn install_index(worktree: &Path, expected: &[u8], clean: &[u8]) -> Result<()> {
    let index = index_path(worktree)?;
    let lock = index.with_file_name("index.lock");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .with_context(|| format!("locking live Git index {}", lock.display()))?;
    file.write_all(INDEX_LOCK_MARKER)?;
    file.sync_all()?;
    drop(file);
    let lock = IndexLock(lock);
    if fs::read(&index).context("re-reading live Git index")? != expected {
        bail!("index changed while salvage was completing; preserved ref left intact")
    }
    crate::write_atomic(&index, clean).context("installing preserved clean Git index")?;
    fs::remove_file(&lock.0).context("releasing live Git index lock")?;
    Ok(())
}

pub(super) fn add_worktree(worktree: &Path, index: &Path) -> Result<()> {
    add_worktree_with(worktree, index, None)
}

pub(super) fn add_worktree_isolated(
    worktree: &Path,
    index: &Path,
    objects: &Objects,
) -> Result<()> {
    add_worktree_with(worktree, index, Some(objects))
}

fn add_worktree_with(worktree: &Path, index: &Path, objects: Option<&Objects>) -> Result<()> {
    // Safety boundary: `git add` writes only this disposable GIT_INDEX_FILE.
    let mut present = Vec::new();
    for raw in bytes(worktree, &["ls-files", "--cached", "-z"], Some(index))?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = repository_path(raw)?;
        match fs::symlink_metadata(worktree.join(&relative)) {
            Ok(_) => {
                present.extend_from_slice(raw);
                present.push(0);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading tracked path {}", relative.display()));
            }
        }
    }
    if !present.is_empty() {
        run_with(
            worktree,
            &[
                "--literal-pathspecs",
                "add",
                "--renormalize",
                "--sparse",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(index),
            Some(&present),
            objects,
        )
        .context("rehashing present tracked files in a disposable Git index")?;
    }
    run_with(
        worktree,
        &["add", "--sparse", "-A", "--", "."],
        Some(index),
        None,
        objects,
    )
    .context("capturing worktree files in a disposable Git index")
}

fn repository_path(raw: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    let path = PathBuf::from(OsString::from_vec(raw.to_vec()));
    #[cfg(not(unix))]
    let path = PathBuf::from(
        String::from_utf8(raw.to_vec()).context("Git returned a non-UTF-8 tracked path")?,
    );
    if !path
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        bail!("Git returned unsafe tracked path {}", path.display())
    }
    Ok(path)
}

pub(super) fn commit_tree(
    worktree: &Path,
    tree: &str,
    parent: &str,
    message: &str,
) -> Result<String> {
    let mut command = command(worktree, &["commit-tree", tree, "-p", parent], None);
    command
        .env("GIT_AUTHOR_NAME", "Grove")
        .env("GIT_AUTHOR_EMAIL", "grove@localhost")
        .env("GIT_COMMITTER_NAME", "Grove")
        .env("GIT_COMMITTER_EMAIL", "grove@localhost");
    Ok(
        String::from_utf8(execute(command, Some(message.as_bytes()))?)?
            .trim()
            .to_string(),
    )
}

pub(super) fn status(worktree: &Path) -> Result<Vec<u8>> {
    let mut command = command(
        worktree,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        None,
    );
    command.env("GIT_OPTIONAL_LOCKS", "0");
    execute(command, None)
}

pub(super) fn optional_text(worktree: &Path, args: &[&str]) -> Result<Option<String>> {
    optional_bytes(worktree, args)?
        .map(String::from_utf8)
        .transpose()
        .map(|value| value.map(|text| text.trim().to_string()))
        .map_err(Into::into)
}

fn optional_bytes(worktree: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let output = command(worktree, args, None)
        .output()
        .with_context(|| format!("spawning git {args:?}"))?;
    match output.status.code() {
        Some(0) => Ok(Some(output.stdout)),
        Some(1) => Ok(None),
        _ => bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

pub(super) fn text(worktree: &Path, args: &[&str], index: Option<&Path>) -> Result<String> {
    Ok(String::from_utf8(bytes(worktree, args, index)?)?
        .trim()
        .to_string())
}

pub(super) fn text_isolated(
    worktree: &Path,
    args: &[&str],
    index: Option<&Path>,
    objects: &Objects,
) -> Result<String> {
    Ok(String::from_utf8(execute(
        command_with(worktree, args, index, Some(objects)),
        None,
    )?)?
    .trim()
    .to_string())
}

pub(super) fn bytes(worktree: &Path, args: &[&str], index: Option<&Path>) -> Result<Vec<u8>> {
    execute(command(worktree, args, index), None)
}

pub(super) fn bytes_input(
    worktree: &Path,
    args: &[&str],
    index: Option<&Path>,
    input: &[u8],
) -> Result<Vec<u8>> {
    execute(command(worktree, args, index), Some(input))
}

pub(super) fn run(
    worktree: &Path,
    args: &[&str],
    index: Option<&Path>,
    input: Option<&[u8]>,
) -> Result<()> {
    run_with(worktree, args, index, input, None)
}

fn run_with(
    worktree: &Path,
    args: &[&str],
    index: Option<&Path>,
    input: Option<&[u8]>,
    objects: Option<&Objects>,
) -> Result<()> {
    execute(command_with(worktree, args, index, objects), input).map(|_| ())
}

fn command(worktree: &Path, args: &[&str], index: Option<&Path>) -> Command {
    command_with(worktree, args, index, None)
}

fn command_with(
    worktree: &Path,
    args: &[&str],
    index: Option<&Path>,
    objects: Option<&Objects>,
) -> Command {
    let mut command = Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .args(args)
        .current_dir(worktree);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    if let Some(objects) = objects {
        command
            .env("GIT_OBJECT_DIRECTORY", &objects.directory)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &objects.alternates);
    }
    command
}

fn execute(mut command: Command, input: Option<&[u8]>) -> Result<Vec<u8>> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let detail = format!("{command:?}");
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {detail}"))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("Git stdin was not piped")?
            .write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "{detail} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(output.stdout)
}
