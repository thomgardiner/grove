//! Prewarm daemon. Seeds every worktree's lane from the canonical *before* an agent
//! builds, so the first build in a fresh worktree is already warm — then watches the
//! repo's worktree directory and seeds new worktrees the moment they appear. This is
//! what turns "spin up an agent worktree" into a zero-wait build.

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use crate::{cache, project, worktree};

fn git(workspace: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .with_context(|| format!("git {args:?}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Absolute paths of every worktree of the repo containing `workspace`, symlink-
/// resolved so they match the workspace path a build derives (see `cache::canonical_path`).
fn worktree_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    Ok(git(workspace, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|p| cache::canonical_path(Path::new(p)))
        .collect())
}

/// The repo's worktree metadata dir (`<git-common-dir>/worktrees`), which gains a
/// subdirectory every time `git worktree add` runs.
fn worktrees_meta_dir(workspace: &Path) -> Result<PathBuf> {
    let common = git(workspace, &["rev-parse", "--git-common-dir"])?
        .trim()
        .to_string();
    Ok(workspace.join(common).join("worktrees"))
}

/// Seed every cold, idle worktree lane from the canonical. Skips lanes an agent is
/// building (their lock is held) and lanes that are already warm. Returns the
/// workspace paths it seeded.
pub fn prewarm(root: &Path, workspace: &Path, repo: &str) -> Result<Vec<String>> {
    let mut seeded = Vec::new();
    for worktree in worktree_paths(workspace)? {
        // Each worktree may pin its own toolchain; seed the lane a build there reads.
        let toolchain = project::toolchain(&worktree);
        let canonical = cache::canonical_dir(root, repo, &toolchain);
        if !canonical.exists() {
            continue;
        }
        let ws = worktree.to_string_lossy().into_owned();
        if let Some(lane) = cache::try_acquire(root, &ws, &toolchain)? {
            if cache::seed(root, &lane, &canonical)? {
                seeded.push(ws);
            }
        }
    }
    Ok(seeded)
}

/// Prewarm all worktrees now, then watch for new ones and prewarm them on arrival.
/// Runs until interrupted.
pub fn watch(root: &Path, workspace: &Path, repo: &str) -> Result<()> {
    let report = |seeded: &[String]| {
        for ws in seeded {
            eprintln!("grove: prewarmed {ws}");
        }
    };
    report(&prewarm(root, workspace, repo)?);

    let meta_dir = worktrees_meta_dir(workspace)?;
    std::fs::create_dir_all(&meta_dir).ok();
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&meta_dir, RecursiveMode::NonRecursive)?;
    eprintln!("grove: watching {} for new worktrees", meta_dir.display());

    // A new worktree seeds it; a quiet interval reaps abandoned ones. On an event,
    // drain the short burst a `git worktree add` writes, then re-seed. On the idle
    // timeout, reclaim worktrees agents walked away from.
    let reap_interval = Duration::from_secs(300);
    loop {
        match rx.recv_timeout(reap_interval) {
            Ok(_) => {
                while rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
                report(&prewarm(root, workspace, repo)?);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                match worktree::reap(root, workspace, worktree::reap_ttl(), false) {
                    Ok(report) => {
                        for w in &report.reaped {
                            eprintln!("grove: reaped abandoned worktree {} ({})", w.path, w.reason);
                        }
                    }
                    Err(e) => eprintln!("grove: reap failed: {e:#}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}
