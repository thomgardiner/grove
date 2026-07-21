use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static SCRATCH_ID: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    root: PathBuf,
    objects: PathBuf,
    index: PathBuf,
}

impl Scratch {
    fn create() -> Result<Self> {
        for _ in 0..64 {
            let id = SCRATCH_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "grove-snapshot-read-only-{}-{id}",
                std::process::id()
            ));
            match create_private_dir(&root) {
                Ok(()) => {
                    let objects = root.join("objects");
                    fs::create_dir(&objects).context("creating snapshot object directory")?;
                    let index = root.join("index");
                    return Ok(Self {
                        root,
                        objects,
                        index,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("creating snapshot scratch directory"),
            }
        }
        bail!("could not allocate snapshot scratch state")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(super) fn index_tree(workspace: &Path) -> Result<String> {
    reject_split_index(workspace)?;
    let source_index = git_path(workspace, "index")?;
    let before = read_optional(&source_index)?;
    let scratch = Scratch::create()?;
    if let Some(bytes) = &before {
        fs::write(&scratch.index, bytes).context("copying source Git index")?;
    }
    let source_objects = fs::canonicalize(git_path(workspace, "objects")?)
        .context("resolving source Git object directory")?;
    let alternates =
        std::env::join_paths([source_objects]).context("encoding source Git object directory")?;
    let output = Command::new("git")
        .args(["write-tree"])
        .current_dir(workspace)
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_INDEX_FILE", &scratch.index)
        .env("GIT_OBJECT_DIRECTORY", &scratch.objects)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates)
        .output()
        .context("capturing Git index tree without source writes")?;
    if read_optional(&source_index)? != before {
        bail!("source Git index changed while capturing its tree")
    }
    tree(output)
}

fn reject_split_index(workspace: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--shared-index-path"])
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("checking for a split Git index")?;
    if !output.status.success() {
        bail!("Git cannot report split-index state")
    }
    if !output.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        bail!("read-only snapshots do not support a split Git index")
    }
    Ok(())
}

fn git_path(workspace: &Path, name: &str) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("locating source Git {name}"))?;
    if !output.status.success() {
        bail!("locating source Git {name} failed")
    }
    let path = output_path(output.stdout)?;
    if path.as_os_str().is_empty() {
        bail!("source Git {name} path is empty")
    }
    Ok(if path.is_absolute() {
        path
    } else {
        workspace.join(&path)
    })
}

#[cfg(unix)]
fn output_path(mut bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        bytes.pop();
    }
    Ok(std::ffi::OsString::from_vec(bytes).into())
}

#[cfg(not(unix))]
fn output_path(bytes: Vec<u8>) -> Result<PathBuf> {
    let value = String::from_utf8(bytes).context("source Git path is not UTF-8")?;
    Ok(value.trim_end_matches(['\r', '\n']).into())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading Git index {}", path.display())),
    }
}

fn tree(output: Output) -> Result<String> {
    if !output.status.success() {
        bail!(
            "capturing Git index tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Git returned an invalid index tree")
    }
    Ok(tree)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(test)]
#[path = "snapshot_read_only_tests.rs"]
mod tests;
