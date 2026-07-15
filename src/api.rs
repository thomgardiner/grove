//! Ergonomic entry point. Most callers want "the cache, for this workspace" — a handle
//! that resolves the cache root, workspace, toolchain, and repo identity once, so the
//! common operations read as method calls instead of threading four values through every
//! call. The binary drives its build and cache commands through this; a downstream Rust
//! program can too.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::{cache, project};

/// A handle to the grove cache bound to one workspace.
pub struct Grove {
    root: PathBuf,
    workspace: PathBuf,
    toolchain: String,
    repo: String,
}

impl Grove {
    /// Open grove for the workspace containing the current directory, using the resolved
    /// cache root (env, config, or the default under `CARGO_HOME`).
    pub fn here() -> Result<Self> {
        Ok(Self::open(&std::env::current_dir()?))
    }

    /// Open grove for the workspace containing `dir`, using the resolved cache root.
    pub fn open(dir: &Path) -> Self {
        Self::with_root(cache::cache_root(), dir)
    }

    /// Open grove with an explicit cache root — for tests, or a caller that manages its
    /// own cache location rather than the resolved default.
    pub fn with_root(root: PathBuf, dir: &Path) -> Self {
        let workspace = project::workspace(dir);
        let toolchain = project::toolchain(&workspace);
        Self::resolved(root, workspace, toolchain)
    }

    /// Open Grove for an arbitrary command, honoring a direct `cargo +toolchain`
    /// selector before choosing the lane and canonical.
    pub fn with_root_for_command(root: PathBuf, dir: &Path, command: &[String]) -> Self {
        let workspace = project::workspace(dir);
        let toolchain = project::command_toolchain(&workspace, command);
        Self::resolved(root, workspace, toolchain)
    }

    /// Open Grove for a command set, giving mixed direct Cargo selectors their own lane.
    pub fn with_root_for_commands<'a>(
        root: PathBuf,
        dir: &Path,
        commands: impl IntoIterator<Item = &'a [String]>,
    ) -> Self {
        let workspace = project::workspace(dir);
        let toolchain = project::commands_toolchain(&workspace, commands);
        Self::resolved(root, workspace, toolchain)
    }

    fn resolved(root: PathBuf, workspace: PathBuf, toolchain: String) -> Self {
        let repo = project::repo_identity(&workspace);
        Self {
            root,
            workspace,
            toolchain,
            repo,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn toolchain(&self) -> &str {
        &self.toolchain
    }

    /// The repo identity the canonical is keyed by — the same for every worktree of the
    /// repo, so they all seed from one warm canonical.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The canonical this workspace seeds from.
    pub fn canonical(&self) -> PathBuf {
        cache::canonical_dir(&self.root, &self.repo, &self.toolchain)
    }

    /// Acquire this workspace's build lane, blocking until its lock is free.
    pub fn lane(&self) -> Result<cache::Lane> {
        cache::acquire(
            &self.root,
            &self.workspace.to_string_lossy(),
            &self.toolchain,
        )
    }

    /// Acquire an independent tagged lane (e.g. a long-running `verify`) that does not
    /// contend with the interactive build lane.
    pub fn tagged_lane(&self, tag: &str) -> Result<cache::Lane> {
        cache::acquire_tagged(
            &self.root,
            &self.workspace.to_string_lossy(),
            &self.toolchain,
            tag,
        )
    }

    /// Acquire the build lane and seed it copy-on-write from the canonical.
    pub fn seeded_lane(&self) -> Result<cache::Lane> {
        let lane = self.lane()?;
        cache::seed(&self.root, &lane, &self.canonical())?;
        Ok(lane)
    }

    /// Acquire a tagged lane and seed it copy-on-write from the canonical.
    pub fn seeded_tagged_lane(&self, tag: &str) -> Result<cache::Lane> {
        let lane = self.tagged_lane(tag)?;
        cache::seed(&self.root, &lane, &self.canonical())?;
        Ok(lane)
    }

    /// Publish a warmed lane as the canonical every lane seeds from.
    pub fn promote(&self, lane: &cache::Lane) -> Result<()> {
        cache::promote(&self.root, lane, &self.canonical())
    }

    /// Reclaim stale lanes and evict to the disk watermark and canonical budget.
    pub fn gc(&self) -> cache::GcReport {
        cache::gc(&self.root)
    }
}
