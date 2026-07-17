//! Cargo selector adapter for the language-neutral claim registry.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::task;
use grove_core::claim as core;
pub use grove_core::claim::{
    Claim, ClaimOutcome, DEFAULT_CLAIM_TTL_SECS, ReleaseOutcome, path_overlap, quarantine_corrupt,
    registry_lock,
};

#[path = "claim_scope.rs"]
pub(crate) mod claim_scope;
pub use claim_scope::resolve_scopes;

/// Compatibility request retaining Cargo-aware scope selection at the Rust boundary.
pub struct ClaimRequest<'a> {
    pub root: &'a Path,
    pub repo: &'a str,
    pub workspace: Option<&'a Path>,
    pub agent: String,
    pub task: String,
    pub scope: Vec<String>,
    pub branch: Option<String>,
    pub force: bool,
}

pub fn claim_ttl() -> u64 {
    let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    crate::config::Config::resolve(&workspace).claim()
}

pub fn ttl(workspace: &Path) -> u64 {
    crate::config::Config::resolve(workspace).claim()
}

fn normalized(workspace: Option<&Path>, scope: &[String]) -> Result<Vec<String>> {
    match workspace {
        Some(workspace) => resolve_scopes(workspace, scope),
        None if scope.iter().any(|scope| scope.starts_with("crate:")) => {
            bail!("crate claim scopes require a workspace")
        }
        None => scope
            .iter()
            .map(|scope| grove_core::scope::normalize(scope))
            .collect(),
    }
}

fn validate(repo: &str, workspace: &Path) -> Result<PathBuf> {
    let workspace = crate::cache::canonical_path(workspace);
    let actual = crate::project::repo_identity(&workspace);
    if crate::cache::canonical_path(Path::new(repo))
        != crate::cache::canonical_path(Path::new(&actual))
    {
        bail!(
            "workspace {} belongs to repository {actual:?}, not {repo:?}",
            workspace.display()
        )
    }
    Ok(workspace)
}

fn resolved_ttl(repo: &str, workspace: Option<&Path>) -> Result<u64> {
    match workspace {
        Some(workspace) => Ok(crate::config::Config::resolve(&validate(repo, workspace)?).claim()),
        None => std::env::var("GROVE_CLAIM_TTL_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .context(
                "standalone claim expiry requires a workspace when GROVE_CLAIM_TTL_SECS is unset",
            ),
    }
}

pub fn claim(req: &ClaimRequest<'_>) -> Result<ClaimOutcome> {
    let ttl = resolved_ttl(req.repo, req.workspace)?;
    let scope = normalized(req.workspace, &req.scope)?;
    core::claim(&core::ClaimRequest {
        root: req.root,
        repo: req.repo,
        ttl,
        agent: req.agent.clone(),
        task: req.task.clone(),
        scope,
        requested_scope: Some(req.scope.clone()),
        branch: req.branch.clone(),
        force: req.force,
    })
}

fn task_conflicts(
    root: &Path,
    repo: &str,
    workspace: Option<&Path>,
    ttl: u64,
    scope: &[String],
    ignore_id: Option<&str>,
    group: Option<&str>,
) -> Result<Vec<Claim>> {
    let mut conflicts = core::conflicts_unlocked(root, repo, ttl, scope, ignore_id, group)?;
    for claim in task::live_claims(root, repo)? {
        let resolved = if claim.resolved_scope.is_empty() {
            normalized(workspace, &claim.scope)?
        } else {
            claim.resolved_scope.clone()
        };
        let shared = group.is_some() && claim.group.as_deref() == group;
        let overlap = resolved
            .iter()
            .any(|x| scope.iter().any(|y| path_overlap(x, y)));
        if ignore_id != Some(claim.id.as_str()) && !shared && overlap {
            conflicts.push(claim);
        }
    }
    Ok(conflicts)
}

pub(crate) fn conflicts_unlocked(
    root: &Path,
    repo: &str,
    workspace: Option<&Path>,
    ttl: u64,
    scope: &[String],
    ignore_id: Option<&str>,
    group: Option<&str>,
) -> Result<Vec<Claim>> {
    task_conflicts(root, repo, workspace, ttl, scope, ignore_id, group)
}

pub(crate) fn conflicts(
    root: &Path,
    repo: &str,
    workspace: &Path,
    scope: &[String],
    ignore_id: &str,
) -> Result<Vec<Claim>> {
    let ttl = resolved_ttl(repo, Some(workspace))?;
    let _lock = registry_lock(root, repo)?;
    task_conflicts(
        root,
        repo,
        Some(workspace),
        ttl,
        scope,
        Some(ignore_id),
        None,
    )
}

pub fn release(
    root: &Path,
    repo: &str,
    workspace: Option<&Path>,
    agent: &str,
    scope: &[String],
) -> Result<ReleaseOutcome> {
    if let Some(workspace) = workspace {
        validate(repo, workspace)?;
    }
    let scope = normalized(workspace, scope)?;
    core::release(root, repo, agent, &scope)
}

pub fn status(root: &Path, repo: &str, workspace: &Path) -> Result<Vec<Claim>> {
    let mut claims = core::status(root, repo, resolved_ttl(repo, Some(workspace))?)?;
    claims.extend(task::live_claims(root, repo)?);
    claims.sort_by_key(|claim| std::cmp::Reverse(claim.created_at));
    Ok(claims)
}
