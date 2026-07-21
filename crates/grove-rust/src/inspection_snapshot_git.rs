//! Private Git repository construction for inspection capsules.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::{materialization_git, snapshot};

struct Isolation {
    root: PathBuf,
    config: PathBuf,
    template: PathBuf,
}

impl Isolation {
    fn create(root: &Path) -> Result<Self> {
        let root = root.join(".git-isolation");
        create_private_dir(&root).context("creating isolated Git environment")?;
        let config = root.join("config");
        fs::write(&config, b"").context("creating empty global Git config")?;
        let template = root.join("template");
        fs::create_dir(&template).context("creating empty Git template")?;
        Ok(Self {
            root,
            config,
            template,
        })
    }

    fn command(&self) -> Command {
        let mut command = command();
        command
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
            ]);
        command
    }
}

impl Drop for Isolation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(super) fn validate_source(source: &Path, state: &Path) -> Result<()> {
    let isolation = Isolation::create(state)?;
    let shared = capture(&isolation, source, &["rev-parse", "--shared-index-path"])?;
    if !shared.is_empty() {
        bail!("inspection capsules do not support split-index repositories")
    }
    validate_index_flags(&isolation, source)?;
    validate_sparse_index(&isolation, source)?;
    branch(&isolation, source).map(|_| ())
}

fn validate_index_flags(isolation: &Isolation, source: &Path) -> Result<()> {
    let output = bytes(isolation, source, &["ls-files", "--debug", "-z"])?;
    let flags: Vec<_> = output
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let marker = b"flags: ";
            let offset = line.windows(marker.len()).position(|part| part == marker)?;
            line.get(offset + marker.len()..)
        })
        .collect();
    let entries = bytes(isolation, source, &["ls-files", "--cached", "-z"])?
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .count();
    if flags.len() != entries {
        bail!("Git returned malformed index flag evidence")
    }
    if flags.iter().any(|flag| *flag != b"0") {
        bail!(
            "inspection capsules reject assume-unchanged, skip-worktree, sparse, and intent-to-add index flags"
        )
    }
    Ok(())
}

fn validate_sparse_index(isolation: &Isolation, source: &Path) -> Result<()> {
    let output = bytes(
        isolation,
        source,
        &["ls-files", "--sparse", "--stage", "-z"],
    )?;
    if output
        .split(|byte| *byte == 0)
        .any(|record| record.starts_with(b"040000 "))
    {
        bail!("inspection capsules do not support a sparse Git index")
    }
    Ok(())
}

pub(super) fn materialize(source: &Path, capsule: &Path, start: &snapshot::Snapshot) -> Result<()> {
    let root = capsule
        .parent()
        .context("inspection capsule has no parent")?;
    let isolation = Isolation::create(root)?;
    let head = start.head()?;
    let branch = branch(&isolation, source)?;
    let mut clone = isolation.command();
    clone
        .arg("clone")
        .args(["--no-local", "--no-hardlinks", "--no-checkout", "--no-tags"])
        .arg("--single-branch")
        .arg("--branch")
        .arg(branch)
        .arg("--template")
        .arg(&isolation.template)
        .arg("--")
        .arg(materialization_git::argument(source))
        .arg(materialization_git::argument(capsule));
    checked(&mut clone, "cloning inspection repository")?;
    run(
        &isolation,
        capsule,
        &["update-ref", "--no-deref", "HEAD", head],
    )?;
    run(&isolation, capsule, &["remote", "remove", "origin"])?;
    clear_refs(&isolation, capsule)?;
    scrub(&isolation, capsule)?;
    run(&isolation, capsule, &["read-tree", head])?;
    apply(&isolation, capsule, &staged(&isolation, source, head)?)?;
    let tree = capture(&isolation, capsule, &["write-tree"])?;
    if tree != start.index_tree()? {
        bail!("inspection capsule index differs from the captured source")
    }
    independent_with(&isolation, source, capsule)
}

pub(super) fn independent(source: &Path, capsule: &Path) -> Result<()> {
    let root = capsule
        .parent()
        .context("inspection capsule has no parent")?;
    let isolation = Isolation::create(root)?;
    independent_with(&isolation, source, capsule)
}

fn independent_with(isolation: &Isolation, source: &Path, capsule: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(capsule.join(".git")).context("reading inspection Git directory")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("inspection capsule Git directory is not standalone")
    }
    let source_git = common(isolation, source)?;
    let capsule_git = common(isolation, capsule)?;
    if source_git == capsule_git || !capsule_git.starts_with(capsule) {
        bail!("inspection capsule shares its Git common directory")
    }
    if capsule_git.join("objects/info/alternates").exists() {
        bail!("inspection capsule has Git object alternates")
    }
    if !capture(isolation, capsule, &["remote"])?.is_empty()
        || !capture(isolation, capsule, &["for-each-ref", "--format=%(refname)"])?.is_empty()
    {
        bail!("inspection capsule retains remote or shared refs")
    }
    validate_config(source, &capsule_git)?;
    validate_hooks(&capsule_git)?;
    validate_index(isolation, source, capsule, &source_git, &capsule_git)
}

fn branch(isolation: &Isolation, source: &Path) -> Result<String> {
    let branch = capture(
        isolation,
        source,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .context("inspection capsules require a named source branch")?;
    if branch.is_empty() {
        bail!("inspection capsules require a named source branch")
    }
    Ok(branch)
}

fn staged(isolation: &Isolation, source: &Path, head: &str) -> Result<Vec<u8>> {
    bytes(
        isolation,
        source,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--cached",
            head,
            "--",
        ],
    )
    .context("capturing staged inspection diff")
}

fn apply(isolation: &Isolation, capsule: &Path, patch: &[u8]) -> Result<()> {
    if patch.is_empty() {
        return Ok(());
    }
    let mut command = isolation.command();
    let mut child = command
        .args(["apply", "--cached", "--binary", "--whitespace=nowarn", "-"])
        .current_dir(capsule)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("applying staged inspection diff")?;
    child
        .stdin
        .take()
        .context("inspection patch input is unavailable")?
        .write_all(patch)
        .context("writing staged inspection diff")?;
    let output = child
        .wait_with_output()
        .context("waiting for staged inspection diff")?;
    checked_output(output, "applying staged inspection diff").map(|_| ())
}

fn clear_refs(isolation: &Isolation, capsule: &Path) -> Result<()> {
    let refs = capture(isolation, capsule, &["for-each-ref", "--format=%(refname)"])?;
    for name in refs.lines().filter(|name| !name.is_empty()) {
        run(isolation, capsule, &["update-ref", "-d", name])?;
    }
    Ok(())
}

fn scrub(isolation: &Isolation, capsule: &Path) -> Result<()> {
    let git = common(isolation, capsule)?;
    remove_dir(git.join("logs"))?;
    remove_dir(git.join("hooks"))?;
    remove_file(git.join("packed-refs"))?;
    remove_file(git.join("FETCH_HEAD"))
}

fn remove_dir(path: PathBuf) -> Result<()> {
    match fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn remove_file(path: PathBuf) -> Result<()> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn validate_config(source: &Path, git: &Path) -> Result<()> {
    let config = fs::read(git.join("config")).context("reading inspection Git config")?;
    let config = String::from_utf8_lossy(&config);
    let lower = config.to_ascii_lowercase();
    let native = source.to_string_lossy();
    let slashed = native.replace('\\', "/");
    if config.contains(native.as_ref())
        || config.contains(&slashed)
        || lower.contains("[remote ")
        || lower.contains("[credential")
        || lower.contains("extraheader")
        || lower.contains("[include")
        || lower.contains("include.path")
        || lower.contains("hookspath")
    {
        bail!("inspection capsule Git config retains source or executable state")
    }
    Ok(())
}

fn validate_hooks(git: &Path) -> Result<()> {
    match fs::read_dir(git.join("hooks")) {
        Ok(mut entries) => match entries.next().transpose()? {
            Some(_) => bail!("inspection capsule retains Git hooks"),
            None => Ok(()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("reading inspection Git hooks"),
    }
}

fn validate_index(
    isolation: &Isolation,
    source: &Path,
    capsule: &Path,
    source_git: &Path,
    git: &Path,
) -> Result<()> {
    let source_index = git_path(isolation, source, source_git, "index")?;
    let capsule_index = git_path(isolation, capsule, git, "index")?;
    if source_index == capsule_index || !capsule_index.starts_with(git) {
        bail!("inspection capsule shares its Git index")
    }
    Ok(())
}

fn common(isolation: &Isolation, workspace: &Path) -> Result<PathBuf> {
    let value = capture(isolation, workspace, &["rev-parse", "--git-common-dir"])?;
    let path = Path::new(&value);
    fs::canonicalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    })
    .context("resolving Git common directory")
}

fn git_path(isolation: &Isolation, workspace: &Path, git: &Path, name: &str) -> Result<PathBuf> {
    let value = capture(isolation, workspace, &["rev-parse", "--git-path", name])?;
    let path = Path::new(&value);
    fs::canonicalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    })
    .or_else(|_| fs::canonicalize(git.join(name)))
    .with_context(|| format!("resolving Git {name}"))
}

fn run(isolation: &Isolation, dir: &Path, args: &[&str]) -> Result<()> {
    capture(isolation, dir, args).map(|_| ())
}

fn capture(isolation: &Isolation, dir: &Path, args: &[&str]) -> Result<String> {
    let output = bytes(isolation, dir, args)?;
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}

fn bytes(isolation: &Isolation, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut git = isolation.command();
    git.args(args).current_dir(dir);
    let output = checked(&mut git, &format!("running git {args:?}"))?;
    Ok(output.stdout)
}

fn checked(command: &mut Command, what: &str) -> Result<Output> {
    let output = command.output().with_context(|| what.to_string())?;
    checked_output(output, what)
}

fn checked_output(output: Output, what: &str) -> Result<Output> {
    if !output.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(output)
}

fn command() -> Command {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if starts_with(&name, "GIT_CONFIG_") {
            command.env_remove(name);
        }
    }
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_EXTERNAL_DIFF",
    ] {
        command.env_remove(name);
    }
    command
}

fn starts_with(name: &OsStr, prefix: &str) -> bool {
    name.to_string_lossy()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
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
