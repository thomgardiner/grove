//! Git worktree manager for parallel agents.

use crate::cache::{cache_root, repo_slug};
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Marker written into grove-managed worktrees so orphan clean only deletes ours.
pub const WORKTREE_STAMP: &str = ".grove-worktree";

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn stamp_worktree(path: &Path, agent: &str) {
    let _ = std::fs::write(path.join(WORKTREE_STAMP), format!("agent={agent}\n"));
}

/// Default worktree parent (no env): sibling of the repo so `path = "../crate"` deps resolve.
pub fn default_worktree_parent(repo: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    if let (Some(parent), Some(name)) = (canon.parent(), canon.file_name()) {
        return parent.join(format!("{}-worktrees", name.to_string_lossy()));
    }
    cache_root().join(repo_slug(repo)).join("worktrees")
}

/// Parent directory for grove-managed worktrees (`GROVE_WORKTREE_ROOT` or default).
pub fn worktree_parent(repo: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("GROVE_WORKTREE_ROOT") {
        return PathBuf::from(p);
    }
    default_worktree_parent(repo)
}

/// Manages agent worktrees for one repository.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    repo: PathBuf,
    root: PathBuf,
}

impl WorktreeManager {
    pub fn open(repo: &Path) -> Result<Self> {
        Ok(Self {
            repo: std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf()),
            root: worktree_parent(repo),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn same_path(a: &Path, b: &Path) -> bool {
        let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
        let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
        ca == cb
    }

    fn is_registered(&self, path: &Path) -> Result<bool> {
        Ok(self
            .list()?
            .iter()
            .any(|w| Self::same_path(Path::new(&w.path), path)))
    }

    /// Create worktree for `agent`, or return it if already live. `force` recreates.
    pub fn acquire(
        &self,
        agent: &str,
        branch: Option<&str>,
        base: Option<&str>,
        force: bool,
    ) -> Result<PathBuf> {
        let agent_s = sanitize(agent);
        if agent_s.is_empty() {
            bail!("agent id is empty after sanitize");
        }
        let branch = branch
            .map(String::from)
            .unwrap_or_else(|| format!("grove/{agent_s}"));
        let base = base.unwrap_or("HEAD");
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join(&agent_s);

        if path.exists() {
            if self.is_registered(&path)? {
                if !force {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    stamp_worktree(&path, &agent_s);
                    return Ok(path);
                }
                self.release(&path, true)?;
            } else if force {
                std::fs::remove_dir_all(&path)?;
            } else {
                bail!(
                    "path exists and is not a live worktree of this repo; pass --force to replace: {}",
                    path.display()
                );
            }
        }

        let branch_exists = crate::git::ok(
            &self.repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        );

        let path_arg = path.to_string_lossy();
        let output = if branch_exists {
            crate::git::run(&self.repo, &["worktree", "add", path_arg.as_ref(), &branch])?
        } else {
            crate::git::run(
                &self.repo,
                &["worktree", "add", "-b", &branch, path_arg.as_ref(), base],
            )?
        };
        if !output.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        stamp_worktree(&path, &agent_s);
        Ok(path)
    }

    /// Remove worktree (keeps branch). Dirty trees need `force`.
    pub fn release(&self, worktree: &Path, force: bool) -> Result<ReleaseOutcome> {
        let canon = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        if !canon.starts_with(&root) {
            bail!(
                "worktree {} is not under grove root {}",
                worktree.display(),
                self.root.display()
            );
        }
        let dirty = crate::git::run(&canon, &["status", "--porcelain"])?;
        if !dirty.stdout.is_empty() && !force {
            bail!("worktree is dirty; pass --force to discard uncommitted work");
        }

        let branch = crate::git::run(&canon, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|name| !name.is_empty());

        let path = canon.to_string_lossy().into_owned();
        let output = if force {
            crate::git::run(&self.repo, &["worktree", "remove", "--force", &path])?
        } else {
            crate::git::run(&self.repo, &["worktree", "remove", &path])?
        };
        if !output.status.success() {
            bail!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(ReleaseOutcome { branch, path })
    }

    /// List worktrees known to git for this repo.
    pub fn list(&self) -> Result<Vec<WorktreeInfo>> {
        let output = crate::git::run(&self.repo, &["worktree", "list", "--porcelain"])?;
        if !output.status.success() {
            bail!(
                "git worktree list failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut infos = Vec::new();
        let mut cur_path: Option<String> = None;
        let mut cur_branch: Option<String> = None;
        let mut cur_head: Option<String> = None;
        let flush = |infos: &mut Vec<WorktreeInfo>,
                     path: &mut Option<String>,
                     branch: &mut Option<String>,
                     head: &mut Option<String>| {
            if let Some(p) = path.take() {
                infos.push(WorktreeInfo {
                    path: p,
                    branch: branch.take(),
                    head: head.take(),
                });
            }
        };
        for line in text.lines() {
            if line.is_empty() {
                flush(&mut infos, &mut cur_path, &mut cur_branch, &mut cur_head);
                continue;
            }
            if let Some(p) = line.strip_prefix("worktree ") {
                flush(&mut infos, &mut cur_path, &mut cur_branch, &mut cur_head);
                cur_path = Some(p.to_string());
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                cur_head = Some(h.to_string());
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                cur_branch = Some(b.to_string());
            } else if line == "detached" {
                cur_branch = None;
            }
        }
        flush(&mut infos, &mut cur_path, &mut cur_branch, &mut cur_head);
        Ok(infos)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseOutcome {
    pub branch: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}
