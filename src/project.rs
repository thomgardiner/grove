//! Project detection shared by the CLI and the worktree pool: the toolchain a
//! workspace pins, and a stable identity for the repo a canonical is keyed by.
//!
//! These live in the library, not the binary, so a build and the worktree pool
//! derive the *same* lane and canonical keys for the same worktree — if they
//! drifted, the pool would prewarm a lane a build never reads.

use std::path::Path;
use std::process::Command;

/// The toolchain channel a workspace pins (`rust-toolchain.toml`), else the
/// `RUSTUP_TOOLCHAIN` override, else `stable`.
pub fn toolchain(ws: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(ws.join("rust-toolchain.toml")) {
        if let Some(chan) = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("channel")
                .and_then(|rest| rest.split('"').nth(1))
        }) {
            return chan.to_string();
        }
    }
    std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "stable".to_string())
}

/// A stable identity for the repo `ws` belongs to: its canonical shared git directory,
/// which is the same for every worktree of the repo. This is what the canonical is
/// keyed by, so all of a repo's worktrees seed from one warm canonical. Using the
/// canonical common dir (not its parent) keeps the key correct under `--separate-git-dir`
/// and symlinked git dirs, where the parent is not a worktree at all.
pub fn repo_identity(ws: &Path) -> String {
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(ws)
        .output()
    {
        if out.status.success() {
            let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !common.is_empty() {
                return crate::cache::canonical_path(&ws.join(common))
                    .to_string_lossy()
                    .into_owned();
            }
        }
    }
    ws.to_string_lossy().into_owned()
}
