//! Ergonomic entry point. Most callers want "the cache, for this workspace" — a handle
//! that resolves the cache root, workspace, toolchain, and repo identity once, so the
//! common operations read as method calls instead of threading four values through every
//! call. The binary drives its build and cache commands through this; a downstream Rust
//! program can too.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::{cache, project};

/// A handle to the grove cache bound to one workspace.
pub struct Grove {
    root: PathBuf,
    workspace: PathBuf,
    toolchain: String,
    repo: String,
    config: Config,
    policy: cache::Policy,
}

impl Grove {
    /// Open grove for the workspace containing the current directory, using the resolved
    /// cache root (env, config, or the default under `CARGO_HOME`).
    pub fn here() -> Result<Self> {
        Ok(Self::open(&std::env::current_dir()?))
    }

    /// Open grove for the workspace containing `dir`, using the resolved cache root.
    pub fn open(dir: &Path) -> Self {
        let workspace = project::workspace(dir);
        let config = Config::resolve(&workspace);
        let root = config.root();
        Self::bind(root, workspace, config)
    }

    /// Open grove with an explicit cache root — for tests, or a caller that manages its
    /// own cache location rather than the resolved default.
    pub fn with_root(root: PathBuf, dir: &Path) -> Self {
        let workspace = project::workspace(dir);
        let config = Config::resolve(&workspace);
        Self::bind(root, workspace, config)
    }

    /// Bind a caller-owned configuration snapshot to an explicit workspace and root.
    pub fn bind(root: PathBuf, workspace: PathBuf, config: Config) -> Self {
        let workspace = cache::canonical_path(&workspace);
        let toolchain = project::cache_toolchain(&workspace);
        Self::resolved(root, workspace, toolchain, config)
    }

    /// Open Grove for an arbitrary command, honoring a direct `cargo +toolchain`
    /// selector before choosing the lane and canonical.
    pub fn with_root_for_command(root: PathBuf, dir: &Path, command: &[String]) -> Self {
        let workspace = project::workspace(dir);
        let config = Config::resolve(&workspace);
        Self::command(root, workspace, config, command)
    }

    pub fn command(root: PathBuf, workspace: PathBuf, config: Config, command: &[String]) -> Self {
        let workspace = cache::canonical_path(&workspace);
        let toolchain = project::cache_command_toolchain(&workspace, command);
        Self::resolved(root, workspace, toolchain, config)
    }

    /// Open Grove for a command set, giving mixed direct Cargo selectors their own lane.
    pub fn with_root_for_commands<'a>(
        root: PathBuf,
        dir: &Path,
        commands: impl IntoIterator<Item = &'a [String]>,
    ) -> Self {
        let workspace = project::workspace(dir);
        let config = Config::resolve(&workspace);
        Self::commands(root, workspace, config, commands)
    }

    pub fn commands<'a>(
        root: PathBuf,
        workspace: PathBuf,
        config: Config,
        commands: impl IntoIterator<Item = &'a [String]>,
    ) -> Self {
        let workspace = cache::canonical_path(&workspace);
        let toolchain = project::cache_commands_toolchain(&workspace, commands);
        Self::resolved(root, workspace, toolchain, config)
    }

    fn resolved(root: PathBuf, workspace: PathBuf, toolchain: String, config: Config) -> Self {
        let repo = project::repo_identity(&workspace);
        let policy = cache::Policy::resolve(&config);
        Self {
            root,
            workspace,
            toolchain,
            repo,
            config,
            policy,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn config(&self) -> &Config {
        &self.config
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

    /// Whether the canonical was atomically published by a completed promotion.
    pub fn published(&self) -> bool {
        cache::published(&self.root, &self.canonical())
    }

    /// Acquire this workspace's build lane, blocking until its lock is free.
    pub fn lane(&self) -> Result<cache::Lane> {
        cache::acquire_with_policy(
            &self.root,
            &self.workspace.to_string_lossy(),
            &self.toolchain,
            &self.policy,
        )
    }

    /// Acquire an independent tagged lane (e.g. a long-running `verify`) that does not
    /// contend with the interactive build lane.
    pub fn tagged_lane(&self, tag: &str) -> Result<cache::Lane> {
        cache::acquire_tagged_with_policy(
            &self.root,
            &self.workspace.to_string_lossy(),
            &self.toolchain,
            tag,
            &self.policy,
        )
    }

    /// Acquire the build lane and seed it copy-on-write from the canonical.
    pub fn seeded_lane(&self) -> Result<cache::Lane> {
        if !self.published() {
            return self.bootstrap_lane();
        }
        let lane = self.lane()?;
        self.seed(lane)
    }

    /// Acquire a tagged lane and seed it copy-on-write from the canonical.
    pub fn seeded_tagged_lane(&self, tag: &str) -> Result<cache::Lane> {
        if !self.published() {
            return self.bootstrap_lane();
        }
        let lane = self.tagged_lane(tag)?;
        self.seed(lane)
    }

    pub(crate) fn seeded_tagged_lane_until(
        &self,
        tag: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<cache::Lane>> {
        if !self.published() {
            return self.bootstrap_lane_until(cancelled);
        }
        let Some(lane) = cache::acquire_tagged_with_policy_until(
            &self.root,
            &self.workspace.to_string_lossy(),
            &self.toolchain,
            tag,
            &self.policy,
            cancelled,
        )?
        else {
            return Ok(None);
        };
        match cache::seed_published(&self.root, &lane, &self.canonical())? {
            cache::Seed::Unpublished => {
                drop(lane);
                warn_policy_mismatch();
                self.bootstrap_lane_until(cancelled)
            }
            cache::Seed::Warm | cache::Seed::Cloned => Ok(Some(lane)),
        }
    }

    fn seed(&self, lane: cache::Lane) -> Result<cache::Lane> {
        match cache::seed_published(&self.root, &lane, &self.canonical())? {
            cache::Seed::Unpublished => {
                drop(lane);
                warn_policy_mismatch();
                self.bootstrap_lane()
            }
            cache::Seed::Warm | cache::Seed::Cloned => Ok(lane),
        }
    }

    /// Acquire the serialized, persistent fallback used only while this workspace has
    /// no verified canonical. Successful output remains workspace-scoped and unverified.
    /// (See [`warn_policy_mismatch`] for the seeded-lane refusal path.)
    pub fn bootstrap_lane(&self) -> Result<cache::Lane> {
        let lane = cache::acquire_bootstrap_with_policy(
            &self.root,
            &self.workspace.to_string_lossy(),
            &self.toolchain,
            &self.policy,
        )?;
        cache::prepare(&lane)?;
        Ok(lane)
    }

    fn bootstrap_lane_until(&self, cancelled: &dyn Fn() -> bool) -> Result<Option<cache::Lane>> {
        let Some(lane) = cache::acquire_bootstrap_with_policy_until(
            &self.root,
            &self.workspace.to_string_lossy(),
            &self.toolchain,
            &self.policy,
            cancelled,
        )?
        else {
            return Ok(None);
        };
        cache::prepare(&lane)?;
        Ok(Some(lane))
    }

    /// Publish a warmed lane as the canonical every lane seeds from.
    pub fn promote(&self, lane: &cache::Lane) -> Result<()> {
        cache::promote(&self.root, lane, &self.canonical())
    }

    /// Reclaim stale lanes and evict to the disk watermark and canonical budget.
    pub fn gc(&self) -> cache::GcReport {
        cache::gc_with_policy(&self.root, &self.policy)
    }

    pub fn status(&self, details: bool) -> cache::Status {
        cache::status_with_policy(&self.root, &self.policy, details)
    }

    /// Run cache maintenance before and after `work` with this handle's bound policy.
    pub fn maintain<T>(&self, work: impl FnOnce() -> T) -> T {
        cache::maintain_with_policy(&self.root, &self.policy, work)
    }
}

/// A published canonical whose policy differs from this workspace's lane policy is
/// refused fail-closed; say so, or the silent bootstrap fallback rebuilds the world
/// on every command with no visible cause.
fn warn_policy_mismatch() {
    eprintln!(
        "grove: published canonical does not match this workspace's lane policy; \
         using serialized unverified bootstrap lane (run grove doctor to compare \
         cargo config and incremental inputs)"
    );
}
