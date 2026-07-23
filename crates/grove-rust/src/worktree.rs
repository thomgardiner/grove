//! Managed worktrees; only a valid lease authorizes removal of Grove-owned state.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, git};

#[path = "worktree_lease.rs"]
mod worktree_lease;
#[expect(dead_code, reason = "private integration precedes scoped CLI exposure")]
#[path = "worktree_materialization.rs"]
mod worktree_materialization;
pub use worktree_lease::Lease;
use worktree_lease::{activity, containing, find_lease, leases, write_lease};
pub use worktree_materialization::{FallbackReason, MaterializationMode, MaterializationRecord};
#[path = "worktree_materialize.rs"]
mod worktree_materialize;
pub use worktree_materialize::{expand, full};
#[path = "worktree_acquire.rs"]
mod worktree_acquire;
pub use worktree_acquire::{AcquireRequest, acquire, bind, scoped};
use worktree_acquire::{
    current_branch, is_our_leased_worktree, reconcile, repo_context, repo_git_lock, worktree_root,
};

#[path = "worktree_reap.rs"]
mod worktree_reap;
pub use worktree_reap::{ReapReport, Reaped, Skipped, reap};
#[path = "worktree_release.rs"]
mod worktree_release;
pub use worktree_release::{ReleaseOutcome, release};
pub(crate) use worktree_release::{preflight_except, release_except};
#[path = "worktree_salvage.rs"]
mod worktree_salvage;
use worktree_salvage::{preflight as preflight_salvage, salvage_work};

pub const DEFAULT_REAP_TTL_SECS: u64 = 2 * 60 * 60;

/// Set while grove is running a git command under the repository lock, so a git
/// hook that itself invokes `grove git` runs the inner command directly instead
/// of blocking on a lock its own parent already holds.
const GIT_GATE_ENV: &str = "GROVE_GIT_GATE";

/// Run `git <args>` in `workspace`, holding the repository's git lock for the
/// commands that would otherwise race concurrent worktrees on shared `.git`
/// state (config, tags, refs). Reads and per-worktree writes run without it.
/// Returns git's own exit code so a caller can propagate it verbatim.
///
/// This is the same lock grove's worktree plumbing takes, so an agent's writes
/// serialize against grove's as well as against other agents.
pub fn run_serialized_git(root: &Path, workspace: &Path, args: &[String]) -> Result<i32> {
    let already_gated = std::env::var_os(GIT_GATE_ENV).is_some();
    // Hold the lock only for a genuine shared-state writer, and only at the
    // outermost grove-git invocation; a nested one (from a hook) already runs
    // under the parent's lock.
    let _guard = if already_gated || !crate::gitgate::needs_serialization(args) {
        None
    } else {
        let repo = crate::project::repo_identity(workspace);
        Some(repo_git_lock(root, &repo)?)
    };
    let status = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .env(GIT_GATE_ENV, "1")
        .status()
        .context("running git")?;
    Ok(status.code().unwrap_or(1))
}

/// Compatibility lookup for callers that have not yet bound an operation workspace.
pub fn reap_ttl() -> u64 {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::config::Config::resolve(&workspace).reap()
}

pub fn ttl(workspace: &Path) -> u64 {
    crate::config::Config::resolve(workspace).reap()
}

pub fn placement(root: &Path, workspace: &Path, config: &crate::config::Config) -> Result<PathBuf> {
    let context = repo_context(workspace)?;
    Ok(worktree_root(
        config,
        root,
        &context.repo_id,
        &context.main_root,
    ))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Renew a matching lease; an unleased human worktree returns `None`.
pub fn touch(root: &Path, target: &Path) -> Result<Option<Lease>> {
    let workspace = cache::canonical_path(target).to_string_lossy().into_owned();
    let Some((_, initial)) = find_lease(root, &workspace)? else {
        return Ok(None);
    };
    let _git = repo_git_lock(root, &initial.repo)?;
    let Some((_, mut lease)) = find_lease(root, &workspace)? else {
        return Ok(None);
    };
    if lease.repo != initial.repo {
        bail!("lease identity for {workspace} changed while acquiring its lifecycle lock")
    }
    lease.last_activity = now_secs();
    write_lease(root, &lease)?;
    Ok(Some(lease))
}

/// Renew exactly one Grove-managed worktree lease.
pub fn heartbeat(root: &Path, target: &Path) -> Result<Lease> {
    let workspace = cache::canonical_path(target).to_string_lossy().into_owned();
    touch(root, target)?
        .with_context(|| format!("no grove lease for {workspace}; refusing heartbeat"))
}

pub(crate) fn managed(root: &Path, target: &Path) -> Result<bool> {
    let workspace = cache::canonical_path(target).to_string_lossy().into_owned();
    find_lease(root, &workspace).map(|lease| lease.is_some())
}

fn remove_worktree(main_root: &Path, path: &Path) -> Result<()> {
    git::run(main_root, &["worktree", "remove", &path.to_string_lossy()])?;
    let _ = git::run(main_root, &["worktree", "prune"]);
    Ok(())
}

#[derive(Serialize)]
pub struct SquashOutcome {
    pub branch: String,
    pub squashed: usize,
    pub commit: String,
    pub message: String,
}

/// Collapse committed work since the lease base with an atomic compare-and-swap ref move.
pub fn squash(
    root: &Path,
    target: &Path,
    base_override: Option<&str>,
    message: Option<&str>,
) -> Result<SquashOutcome> {
    let ws = cache::canonical_path(target);
    let ws_str = ws.to_string_lossy().into_owned();
    let (_, lease) = find_lease(root, &ws_str)?
        .with_context(|| format!("no grove lease for {ws_str}; refusing to rewrite its history"))?;
    if current_branch(&ws).as_deref() != Some(lease.branch.as_str()) {
        bail!(
            "{ws_str} is not on its leased branch {}; refusing to rewrite",
            lease.branch
        );
    }

    let base = base_override
        .map(str::to_string)
        .or_else(|| (!lease.base_oid.is_empty()).then(|| lease.base_oid.clone()))
        .context("no base to squash onto; pass --base <ref>")?;

    let head = git::capture(&ws, &["rev-parse", "HEAD"])?;
    let fork = git::capture(&ws, &["merge-base", "HEAD", &base])
        .with_context(|| format!("finding the fork point from {base}"))?;
    if fork == head {
        bail!("nothing to squash: the branch has no commits beyond its base");
    }
    let squashed = git::capture(&ws, &["rev-list", "--count", &format!("{fork}..HEAD")])?
        .parse()
        .unwrap_or(0);
    let message = match message {
        Some(m) => m.to_string(),
        None => git::capture(&ws, &["log", "--format=%s", &format!("{fork}..HEAD")])?
            .lines()
            .last()
            .unwrap_or("grove: squashed work")
            .to_string(),
    };

    let _git = repo_git_lock(root, &lease.repo)?;
    let new = git::capture(
        &ws,
        &[
            "commit-tree",
            &format!("{head}^{{tree}}"),
            "-p",
            &fork,
            "-m",
            &message,
        ],
    )?;
    git::run(
        &ws,
        &[
            "update-ref",
            &format!("refs/heads/{}", lease.branch),
            &new,
            &head,
        ],
    )?;
    let commit = git::capture(&ws, &["rev-parse", "--short", &new])?;
    Ok(SquashOutcome {
        branch: lease.branch,
        squashed,
        commit,
        message,
    })
}

#[derive(Serialize)]
pub struct WorktreeInfo {
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialization: Option<MaterializationRecord>,
    pub exists: bool,
    pub dirty: bool,
    pub idle_secs: u64,
    pub age_secs: u64,
}

fn dirty(path: &Path) -> bool {
    Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

pub fn list(root: &Path) -> Vec<WorktreeInfo> {
    let now = now_secs();
    leases(root)
        .into_iter()
        .map(|(_, lease)| {
            let path = PathBuf::from(&lease.workspace);
            let exists = path.exists();
            let dirty = exists && dirty(&path);
            WorktreeInfo {
                repo: lease.repo.clone(),
                exists,
                dirty,
                idle_secs: now.saturating_sub(activity(root, &lease)),
                age_secs: now.saturating_sub(lease.created_at),
                path: lease.workspace,
                branch: lease.branch,
                agent: lease.agent,
                materialization: lease.materialization,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use tempfile::tempdir;

    struct Process {
        cwd: PathBuf,
        worktree_root: Option<OsString>,
        reap_ttl: Option<OsString>,
    }

    impl Process {
        fn isolate() -> Self {
            let process = Self {
                cwd: std::env::current_dir().unwrap(),
                worktree_root: std::env::var_os("GROVE_WORKTREE_ROOT"),
                reap_ttl: std::env::var_os("GROVE_REAP_TTL_SECS"),
            };
            // SAFETY: nextest runs each test in its own process.
            unsafe {
                std::env::remove_var("GROVE_WORKTREE_ROOT");
                std::env::remove_var("GROVE_REAP_TTL_SECS");
            }
            process
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.cwd).unwrap();
            for (key, value) in [
                ("GROVE_WORKTREE_ROOT", self.worktree_root.take()),
                ("GROVE_REAP_TTL_SECS", self.reap_ttl.take()),
            ] {
                // SAFETY: nextest runs each test in its own process.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn repository(path: &Path, root: &str, ttl: u64) {
        fs::create_dir_all(path).unwrap();
        git::run(path, &["init", "-q"]).unwrap();
        git::run(path, &["config", "user.email", "worktree@example.invalid"]).unwrap();
        git::run(path, &["config", "user.name", "worktree-test"]).unwrap();
        fs::write(path.join("file"), "x").unwrap();
        fs::write(path.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\n").unwrap();
        fs::write(
            path.join(".grove.toml"),
            format!("worktree_root = \"../{root}\"\nreap_ttl_secs = {ttl}\n"),
        )
        .unwrap();
        git::run(path, &["add", "-A"]).unwrap();
        git::run(path, &["commit", "-q", "-m", "init"]).unwrap();
    }

    fn acquired(root: &Path, repo: &Path, agent: &str) -> PathBuf {
        acquire(&AcquireRequest {
            root,
            cwd: repo,
            agent: agent.into(),
            branch: Some(format!("grove/{agent}")),
            base: "HEAD".into(),
        })
        .unwrap()
    }

    #[test]
    fn placement_and_reap_ttl_stay_bound_to_each_repository() {
        let base = tempdir().unwrap();
        let _process = Process::isolate();
        let root = base.path().join("cache");
        let a = base.path().join("a");
        let b = base.path().join("b");
        repository(&a, "worktrees-a", 0);
        repository(&b, "worktrees-b", 3600);

        std::env::set_current_dir(&b).unwrap();
        let a_worktree = acquired(&root, &a, "a");
        let b_worktree = acquired(&root, &b, "b");
        assert!(a_worktree.starts_with(cache::canonical_path(&base.path().join("worktrees-a"))));
        assert!(b_worktree.starts_with(cache::canonical_path(&base.path().join("worktrees-b"))));

        let a_first = reap(&root, &a, ttl(&a), true).unwrap();
        let b_second = reap(&root, &b, ttl(&b), true).unwrap();
        std::env::set_current_dir(&a).unwrap();
        let b_first = reap(&root, &b, ttl(&b), true).unwrap();
        let a_second = reap(&root, &a, ttl(&a), true).unwrap();
        assert_eq!((a_first.reaped.len(), a_second.reaped.len()), (1, 1));
        assert_eq!((b_first.reaped.len(), b_second.reaped.len()), (0, 0));
    }

    #[test]
    fn environment_overrides_repository_worktree_policy() {
        let base = tempdir().unwrap();
        let _process = Process::isolate();
        let root = base.path().join("cache");
        let repo = base.path().join("repo");
        let override_root = base.path().join("override");
        repository(&repo, "configured", 0);
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::set_var("GROVE_WORKTREE_ROOT", &override_root);
            std::env::set_var("GROVE_REAP_TTL_SECS", "3600");
        }

        let workspace = acquired(&root, &repo, "override");
        assert!(workspace.starts_with(cache::canonical_path(&override_root)));
        assert!(
            reap(&root, &repo, ttl(&repo), true)
                .unwrap()
                .reaped
                .is_empty()
        );
    }
}
