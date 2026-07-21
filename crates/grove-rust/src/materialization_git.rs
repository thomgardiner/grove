use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

#[path = "materialization_git_process.rs"]
mod process;
use process::{execute, optional, run, run_bytes};

#[derive(Debug)]
pub enum Failure {
    Unsupported(String),
    Setup(String),
}

impl Failure {
    pub fn classify(code: Option<i32>, detail: &str) -> Self {
        let lower = detail.to_ascii_lowercase();
        if code == Some(129)
            || lower.contains("unknown option")
            || lower.contains("unrecognized option")
            || lower.contains("unknown switch")
            || lower.contains("is not a git command")
        {
            Self::Unsupported(detail.into())
        } else {
            Self::Setup(detail.into())
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(detail) => write!(formatter, "unsupported Git operation: {detail}"),
            Self::Setup(detail) => write!(formatter, "Git setup failed: {detail}"),
        }
    }
}

impl std::error::Error for Failure {}

pub struct Add<'a> {
    pub main: &'a Path,
    pub branch: &'a str,
    pub existing: bool,
    pub workspace: &'a Path,
    pub base: &'a str,
    pub checkout: bool,
}

pub fn add(input: &Add<'_>) -> Result<(), Failure> {
    if input.branch.is_empty() || input.base.is_empty() {
        return Err(Failure::Setup(
            "worktree branch and base must not be empty".into(),
        ));
    }
    let mut command = Command::new("git");
    command.current_dir(input.main).args(["worktree", "add"]);
    if !input.checkout {
        command.arg("--no-checkout");
    }
    if input.existing {
        command
            .arg("--")
            .arg(argument(input.workspace))
            .arg(input.branch);
    } else {
        command
            .arg("-b")
            .arg(input.branch)
            .arg("--")
            .arg(argument(input.workspace))
            .arg(input.base);
    }
    execute(&mut command, None, "adding linked worktree").map(|_| ())
}

#[cfg(not(windows))]
pub(crate) fn argument(path: &Path) -> &std::ffi::OsStr {
    path.as_os_str()
}

#[cfg(windows)]
pub(crate) fn argument(path: &Path) -> std::ffi::OsString {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];
    let raw: Vec<_> = path.as_os_str().encode_wide().collect();
    let plain = if raw.starts_with(UNC) {
        [b'\\' as u16, b'\\' as u16]
            .into_iter()
            .chain(raw[UNC.len()..].iter().copied())
            .collect()
    } else if raw.starts_with(VERBATIM) && raw.get(5) == Some(&(b':' as u16)) {
        raw[VERBATIM.len()..].to_vec()
    } else {
        raw
    };
    std::ffi::OsString::from_wide(&plain)
}

pub fn sparse(workspace: &Path, cones: &[String]) -> Result<Vec<String>, Failure> {
    let cones = checked(workspace, cones)?;
    let empty_index = run(
        workspace,
        &["ls-files", "-z"],
        None,
        "checking worktree index",
    )?
    .is_empty();
    let input = format!("{}\n", cones.join("\n"));
    run(
        workspace,
        &[
            "sparse-checkout",
            "set",
            "--cone",
            "--no-sparse-index",
            "--stdin",
        ],
        Some(&input),
        "configuring sparse checkout",
    )?;
    if empty_index {
        run(
            workspace,
            &["read-tree", "-mu", "HEAD"],
            None,
            "populating sparse checkout",
        )?;
    }
    run(
        workspace,
        &["config", "--worktree", "index.sparse", "false"],
        None,
        "disabling sparse index",
    )?;
    listed(workspace)
}

pub fn full(workspace: &Path) -> Result<(), Failure> {
    let sparse = enabled(workspace)?;
    let skipped = skip_worktree(workspace)?;
    if !sparse && !skipped.is_empty() {
        populate_skipped(workspace, &skipped)?;
    }
    if sparse {
        run(
            workspace,
            &["sparse-checkout", "disable"],
            None,
            "disabling sparse checkout",
        )?;
    }
    populate(workspace)?;
    if sparse || !skipped.is_empty() || sparse_index(workspace)? {
        run(
            workspace,
            &["config", "--worktree", "index.sparse", "false"],
            None,
            "disabling sparse index",
        )?;
    }
    if enabled(workspace)? || sparse_index(workspace)? || !skip_worktree(workspace)?.is_empty() {
        return Err(Failure::Setup(
            "full checkout still contains sparse state".into(),
        ));
    }
    Ok(())
}

pub fn cones(workspace: &Path) -> Result<Option<Vec<String>>, Failure> {
    enabled(workspace)?.then(|| listed(workspace)).transpose()
}

fn enabled(workspace: &Path) -> Result<bool, Failure> {
    configured(
        workspace,
        "core.sparseCheckout",
        "reading sparse checkout config",
    )
}

fn sparse_index(workspace: &Path) -> Result<bool, Failure> {
    configured(workspace, "index.sparse", "reading sparse index config")
}

fn configured(workspace: &Path, key: &str, operation: &str) -> Result<bool, Failure> {
    let args = ["config", "--bool", "--get", key];
    match optional(workspace, &args, operation) {
        Ok(value) => Ok(value.as_deref() == Some("true")),
        Err(Failure::Unsupported(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

fn populate(workspace: &Path) -> Result<(), Failure> {
    let empty = run(
        workspace,
        &["ls-files", "-z"],
        None,
        "checking worktree index",
    )?
    .is_empty();
    if empty {
        run(
            workspace,
            &["read-tree", "-mu", "HEAD"],
            None,
            "populating full checkout",
        )?;
    }
    Ok(())
}

fn skip_worktree(workspace: &Path) -> Result<Vec<u8>, Failure> {
    let output = run_bytes(
        workspace,
        &["ls-files", "-t", "-z"],
        None,
        "checking skip-worktree entries",
    )?;
    let mut paths = Vec::new();
    for entry in output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Some(path) = entry.strip_prefix(b"S ") else {
            continue;
        };
        repository_path(path)?;
        paths.extend_from_slice(path);
        paths.push(0);
    }
    Ok(paths)
}

fn populate_skipped(workspace: &Path, skipped: &[u8]) -> Result<(), Failure> {
    let mut missing = Vec::new();
    for raw in skipped
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = repository_path(raw)?;
        match std::fs::symlink_metadata(workspace.join(relative)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.extend_from_slice(raw);
                missing.push(0);
            }
            Err(error) => {
                return Err(Failure::Setup(format!(
                    "checking skipped worktree path: {error}"
                )));
            }
        }
    }
    if !missing.is_empty() {
        run_bytes(
            workspace,
            &[
                "checkout-index",
                "--ignore-skip-worktree-bits",
                "-z",
                "--stdin",
            ],
            Some(&missing),
            "populating skipped worktree files",
        )?;
    }
    run_bytes(
        workspace,
        &["update-index", "--no-skip-worktree", "-z", "--stdin"],
        Some(skipped),
        "clearing skip-worktree entries",
    )?;
    Ok(())
}

fn repository_path(raw: &[u8]) -> Result<PathBuf, Failure> {
    #[cfg(unix)]
    let path = PathBuf::from(OsString::from_vec(raw.to_vec()));
    #[cfg(not(unix))]
    let path = PathBuf::from(String::from_utf8(raw.to_vec()).map_err(|error| {
        Failure::Setup(format!("Git returned a non-UTF-8 tracked path: {error}"))
    })?);
    if !path
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(Failure::Setup(format!(
            "Git returned unsafe tracked path {}",
            path.display()
        )));
    }
    Ok(path)
}

pub fn head(workspace: &Path) -> Result<String, Failure> {
    run(
        workspace,
        &["rev-parse", "--verify", "HEAD"],
        None,
        "reading worktree HEAD",
    )
    .map(|value| value.trim().into())
}

fn checked(workspace: &Path, cones: &[String]) -> Result<Vec<String>, Failure> {
    if cones.is_empty() {
        return Err(Failure::Setup("sparse checkout requires a cone".into()));
    }
    let mut checked = Vec::with_capacity(cones.len());
    for cone in cones {
        validate(cone)?;
        let object = format!("HEAD:{cone}");
        let kind = run(
            workspace,
            &["cat-file", "-t", &object],
            None,
            "checking sparse cone",
        )?;
        if kind.trim() != "tree" {
            return Err(Failure::Setup(format!(
                "sparse cone {cone:?} is not a tracked directory"
            )));
        }
        checked.push(cone.clone());
    }
    checked.sort();
    checked.dedup();
    Ok(checked)
}

fn validate(cone: &str) -> Result<(), Failure> {
    let drive = cone.as_bytes().get(1) == Some(&b':');
    let invalid = cone.is_empty()
        || cone == "."
        || cone.starts_with(['/', '\\'])
        || cone.contains(['\\', '\0', '\n', '\r'])
        || drive
        || cone
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if invalid {
        return Err(Failure::Setup(format!(
            "invalid sparse directory cone {cone:?}"
        )));
    }
    Ok(())
}

fn listed(workspace: &Path) -> Result<Vec<String>, Failure> {
    let output = run(
        workspace,
        &["-c", "core.quotePath=false", "sparse-checkout", "list"],
        None,
        "reading sparse cones",
    )?;
    let mut cones: Vec<_> = output.lines().map(str::to_string).collect();
    for cone in &cones {
        validate(cone)?;
    }
    cones.sort();
    cones.dedup();
    Ok(cones)
}

#[cfg(test)]
include!("materialization_git_tests.rs");
