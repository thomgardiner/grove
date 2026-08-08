//! Thin git process helpers.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub fn run(cwd: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))
}

pub fn stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = run(cwd, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn ok(cwd: &Path, args: &[&str]) -> bool {
    run(cwd, args)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Paths from `git worktree list --porcelain` (fail closed on error).
pub fn worktree_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    let output = run(workspace, &["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree ").map(PathBuf::from))
        .collect())
}
