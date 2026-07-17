//! Thin git command helpers. The worktree pool runs a lot of git, and every call
//! wants the same shape: run in a given directory, fail loudly with stderr on a
//! non-zero exit.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Run `git <args>` in `dir` and return trimmed stdout. Errors if git exits non-zero.
pub fn capture(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("spawning git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run `git <args>` in `dir` for its effect, discarding stdout. Errors on non-zero exit.
pub fn run(dir: &Path, args: &[&str]) -> Result<()> {
    capture(dir, args).map(|_| ())
}
