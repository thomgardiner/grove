//! Cache host: seed target for the primary checkout, hardlink lanes for worktrees.
//!
//! ```text
//! {cache_root}/{repo_slug}/{toolchain}/seed/target/         # primary checkout
//! {cache_root}/{repo_slug}/{toolchain}/lanes/{id}/target/   # linked worktrees
//! ```

mod clean;
mod seed_link;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

pub use clean::{CleanOpts, CleanReport, format_bytes, live_lane_keys};

/// Root for all Grove caches (`GROVE_CACHE` or platform default).
pub fn cache_root() -> PathBuf {
    if let Ok(p) = std::env::var("GROVE_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("grove");
    }
    if let Some(h) = std::env::var_os("HOME") {
        #[cfg(target_os = "macos")]
        {
            return PathBuf::from(h).join("Library/Caches/grove");
        }
        #[cfg(not(target_os = "macos"))]
        {
            return PathBuf::from(h).join(".cache/grove");
        }
    }
    PathBuf::from(".grove-cache")
}

fn git_common_dir(workspace: &Path) -> Result<PathBuf> {
    let raw = crate::git::stdout(
        workspace,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let path = PathBuf::from(raw.trim());
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

fn git_dir(workspace: &Path) -> Result<PathBuf> {
    let raw = crate::git::stdout(
        workspace,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?;
    let path = PathBuf::from(raw.trim());
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

fn repo_display_name(common_dir: &Path) -> String {
    let base = if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir.parent().unwrap_or(common_dir)
    } else {
        common_dir
    };
    base.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

/// Stable short id for a repository (all worktrees share it).
pub fn repo_slug(workspace: &Path) -> String {
    let key = git_common_dir(workspace).unwrap_or_else(|_| {
        std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
    });
    let mut h = Sha256::new();
    h.update(key.to_string_lossy().as_bytes());
    format!(
        "{}-{}",
        repo_display_name(&key),
        &crate::util::hex_lower(h.finalize())[..12]
    )
}

fn toolchain_key() -> Result<String> {
    use std::sync::OnceLock;
    static KEY: OnceLock<String> = OnceLock::new();
    if let Some(k) = KEY.get() {
        return Ok(k.clone());
    }
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("rustc -vV")?;
    if !output.status.success() {
        bail!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut h = Sha256::new();
    h.update(&output.stdout);
    let key = crate::util::hex_lower(h.finalize())[..16].to_string();
    let _ = KEY.set(key.clone());
    Ok(key)
}

/// Main worktree: `git-dir` equals common-dir.
fn is_primary_checkout(workspace: &Path) -> bool {
    let Ok(common) = git_common_dir(workspace) else {
        return true;
    };
    let Ok(gd) = git_dir(workspace) else {
        return true;
    };
    gd == common
}

pub(crate) fn worktree_lane_key(workspace: &Path) -> String {
    let canon = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut h = Sha256::new();
    h.update(canon.to_string_lossy().as_bytes());
    crate::util::hex_lower(h.finalize())[..12].to_string()
}

/// Cache host for one (repository, rustc) pair on this machine.
#[derive(Debug, Clone)]
pub struct CacheHost {
    pub repo_slug: String,
    pub toolchain: String,
    host_root: PathBuf,
}

impl CacheHost {
    /// Open the host for `workspace` (any worktree of the repo).
    pub fn open(workspace: &Path) -> Result<Self> {
        let repo_slug = repo_slug(workspace);
        let toolchain = toolchain_key()?;
        let host_root = cache_root().join(&repo_slug).join(&toolchain);
        std::fs::create_dir_all(&host_root)?;
        Ok(Self {
            repo_slug,
            toolchain,
            host_root,
        })
    }

    pub fn host_root(&self) -> &Path {
        &self.host_root
    }

    pub fn seed_target(&self) -> PathBuf {
        self.host_root.join("seed").join("target")
    }

    fn lane_kind_and_dir(&self, workspace: &Path) -> (LaneKind, PathBuf) {
        if is_primary_checkout(workspace) {
            (LaneKind::Seed, self.seed_target())
        } else {
            (
                LaneKind::Worktree,
                self.host_root
                    .join("lanes")
                    .join(worktree_lane_key(workspace))
                    .join("target"),
            )
        }
    }

    /// Target dir for this checkout: the primary seed or this worktree's hardlinked lane.
    pub fn lane(&self, workspace: &Path) -> Result<Lane> {
        let (kind, target_dir) = self.lane_kind_and_dir(workspace);
        std::fs::create_dir_all(&target_dir)?;
        clean::touch_access(&target_dir);
        if kind == LaneKind::Worktree {
            let seed = self.seed_target();
            let linked = seed_link::link_seed_into_lane(workspace, &seed, &target_dir)?;
            if linked > 0 {
                eprintln!(
                    "grove: linked {linked} seed files → {}",
                    target_dir.display()
                );
            } else if !seed_link::seed_has_artifacts(&seed)
                && !target_dir.join("debug").join("deps").is_dir()
            {
                eprintln!("grove: seed empty — run `grove warm` on the primary first");
            }
        }
        Ok(Lane { target_dir, kind })
    }

    pub fn worktree_lane_key_for(workspace: &Path) -> String {
        worktree_lane_key(workspace)
    }

    /// `cargo check --workspace` into this checkout's lane, then hygiene.
    pub fn warm(&self, workspace: &Path) -> Result<i32> {
        let lane = self.lane(workspace)?;
        eprintln!(
            "grove: warm {} {}",
            if lane.is_seed() { "seed" } else { "lane" },
            lane.target_dir.display()
        );
        // Warm fills the seed; do not pass --locked (edit-loop lockfile bumps must work).
        let code = crate::cargo_cmd::run_cargo(workspace, &lane, &["check", "--workspace"])?;
        if code == 0 {
            log_hygiene(self.hygiene(workspace));
        }
        Ok(code)
    }

    /// Run post-build hygiene and log a one-line summary on reclaim.
    pub fn hygiene_log(&self, workspace: &Path) {
        log_hygiene(self.hygiene(workspace));
    }

    /// Report host paths without materializing a lane.
    pub fn status(&self, workspace: &Path) -> Result<CacheStatus> {
        let (kind, target_dir) = self.lane_kind_and_dir(workspace);
        Ok(CacheStatus {
            workspace: workspace.display().to_string(),
            cache_root: cache_root().display().to_string(),
            repo_slug: self.repo_slug.clone(),
            toolchain: self.toolchain.clone(),
            lane: match kind {
                LaneKind::Seed => "seed".into(),
                LaneKind::Worktree => "worktree".into(),
            },
            target_dir: target_dir.display().to_string(),
        })
    }
}

fn log_hygiene(result: Result<CleanReport>) {
    match result {
        Ok(r) if r.touched() => {
            eprintln!(
                "grove: clean lanes={} seeds={} {}",
                r.lanes_removed,
                r.seeds_removed,
                format_bytes(r.bytes_reclaimed)
            );
            if r.over_budget_bytes > 0 {
                eprintln!(
                    "grove: still {} over GROVE_CACHE_MAX_GB (live seed alone); \
                     raise the cap or `grove clean --seed --force` when idle",
                    format_bytes(r.over_budget_bytes)
                );
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("grove: clean skipped: {e:#}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneKind {
    Seed,
    Worktree,
}

/// A cargo target directory under this host (seed or worktree lane).
#[derive(Debug, Clone)]
pub struct Lane {
    pub target_dir: PathBuf,
    pub kind: LaneKind,
}

impl Lane {
    pub fn is_seed(&self) -> bool {
        self.kind == LaneKind::Seed
    }

    /// Set `CARGO_TARGET_DIR` to this lane and clear `CARGO_BUILD_BUILD_DIR`.
    pub fn apply(&self, cmd: &mut Command) {
        cmd.env("CARGO_TARGET_DIR", &self.target_dir);
        cmd.env_remove("CARGO_BUILD_BUILD_DIR");
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheStatus {
    pub workspace: String,
    pub cache_root: String,
    pub repo_slug: String,
    pub toolchain: String,
    pub lane: String,
    pub target_dir: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_lane_differs_from_seed() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(crate::git::ok(&repo, &["init", "-b", "main"]));
        assert!(crate::git::ok(
            &repo,
            &["config", "user.email", "t@example.com"]
        ));
        assert!(crate::git::ok(&repo, &["config", "user.name", "t"]));
        std::fs::write(repo.join("README"), "x").unwrap();
        assert!(crate::git::ok(&repo, &["add", "README"]));
        assert!(crate::git::ok(&repo, &["commit", "-m", "init"]));

        let wt = root.path().join("wt");
        assert!(crate::git::ok(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "-b", "agent/a"],
        ));

        let host = CacheHost::open(&repo).unwrap();
        assert_eq!(host.repo_slug, CacheHost::open(&wt).unwrap().repo_slug);

        let main = host.lane(&repo).unwrap();
        let linked = host.lane(&wt).unwrap();
        assert!(main.is_seed());
        assert!(!linked.is_seed());
        assert_ne!(main.target_dir, linked.target_dir);
    }
}
