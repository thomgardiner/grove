//! Work coordination (opt-in). Agents declare what part of the repo they are working on
//! so a swarm can pick non-overlapping work and not clobber each other. A claim is
//! grove's lease model — an exclusive flock plus a TTL — pointed at a scope of the repo
//! (paths or crates) instead of a worktree.
//!
//! `claim` is first-wins: it takes the registry lock, rejects a scope that overlaps a
//! live claim from another agent, and otherwise records the claim. `status` is the board
//! agents read to decide what to work on. Claims expire on a TTL, like abandoned
//! worktrees, so a dead agent never holds work forever.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cache;

/// A claim not renewed within this long is treated as abandoned and ignored.
pub const DEFAULT_CLAIM_TTL_SECS: u64 = 30 * 60;

pub fn claim_ttl() -> u64 {
    std::env::var("GROVE_CLAIM_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(crate::config::get().claim_ttl_secs)
        .unwrap_or(DEFAULT_CLAIM_TTL_SECS)
}

/// One agent's declared work: a scope of the repo it is actively changing. Scope entries
/// are repo-relative path prefixes, or `crate:<name>` for a whole crate.
#[derive(Serialize, Deserialize, Clone)]
pub struct Claim {
    pub id: String,
    pub agent: String,
    pub task: String,
    pub scope: Vec<String>,
    pub branch: Option<String>,
    pub created_at: u64,
}

pub struct ClaimRequest<'a> {
    pub root: &'a Path,
    pub repo: &'a str,
    pub agent: String,
    pub task: String,
    pub scope: Vec<String>,
    pub branch: Option<String>,
    pub force: bool,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum ClaimOutcome {
    Granted {
        claim: Claim,
    },
    /// The requested scope overlaps live claims held by other agents.
    Conflict {
        requested: Vec<String>,
        conflicts: Vec<Claim>,
    },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn claims_dir(root: &Path, repo: &str) -> PathBuf {
    root.join("claims").join(cache::repo_slug(repo))
}

/// Serialize claim/release so the overlap check and the write are one atomic step; two
/// agents racing for overlapping scopes then resolve to exactly one winner.
pub(crate) fn registry_lock(root: &Path, repo: &str) -> Result<File> {
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    let file = File::create(locks.join(format!("claims-{}.lock", cache::repo_slug(repo))))
        .context("opening claim-registry lock")?;
    file.lock_exclusive().context("locking claim registry")?;
    Ok(file)
}

fn read_claims(dir: &Path) -> Vec<(PathBuf, Claim)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let claim: Claim = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
            Some((p, claim))
        })
        .collect()
}

/// Two scopes overlap if any of their entries do. Paths overlap on directory containment;
/// crates overlap only with the same crate. A path spec and a crate spec are treated as
/// disjoint (resolving a crate to its path is a later refinement).
fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.iter().any(|y| specs_overlap(x, y)))
}

fn live_standalone(root: &Path, repo: &str) -> Vec<Claim> {
    let now = now_secs();
    let ttl = claim_ttl();
    let dir = claims_dir(root, repo);
    read_claims(&dir)
        .into_iter()
        .filter_map(|(path, claim)| {
            if now.saturating_sub(claim.created_at) <= ttl {
                Some(claim)
            } else {
                let _ = fs::remove_file(path);
                None
            }
        })
        .collect()
}

pub(crate) fn conflicts_unlocked(
    root: &Path,
    repo: &str,
    scope: &[String],
    ignore_agent: Option<&str>,
) -> Result<Vec<Claim>> {
    Ok(live_standalone(root, repo)
        .into_iter()
        .chain(crate::task::live_claims(root, repo)?)
        .filter(|claim| {
            ignore_agent != Some(claim.agent.as_str()) && scopes_overlap(&claim.scope, scope)
        })
        .collect())
}

fn specs_overlap(x: &str, y: &str) -> bool {
    match (x.strip_prefix("crate:"), y.strip_prefix("crate:")) {
        (Some(cx), Some(cy)) => cx == cy,
        (None, None) => path_overlap(x, y),
        _ => false,
    }
}

fn path_overlap(x: &str, y: &str) -> bool {
    let x = x.trim_matches('/');
    let y = y.trim_matches('/');
    x == y || y.starts_with(&format!("{x}/")) || x.starts_with(&format!("{y}/"))
}

/// Claim a scope of the repo for `agent`. First-wins: rejects a scope overlapping another
/// agent's live claim unless `force`. Renewing the same agent's overlapping claim is fine.
pub fn claim(req: &ClaimRequest) -> Result<ClaimOutcome> {
    let dir = claims_dir(req.root, req.repo);
    fs::create_dir_all(&dir)?;
    let _lock = registry_lock(req.root, req.repo)?;

    let now = now_secs();
    let conflicts = conflicts_unlocked(req.root, req.repo, &req.scope, Some(&req.agent))?;
    if !conflicts.is_empty() && !req.force {
        return Ok(ClaimOutcome::Conflict {
            requested: req.scope.clone(),
            conflicts,
        });
    }

    let claim = Claim {
        id: cache::repo_slug(&format!("{}|{}|{}", req.agent, req.scope.join(","), now)),
        agent: req.agent.clone(),
        task: req.task.clone(),
        scope: req.scope.clone(),
        branch: req.branch.clone(),
        created_at: now,
    };
    cache::write_atomic(
        &dir.join(format!("{}.json", claim.id)),
        &serde_json::to_vec_pretty(&claim)?,
    )?;
    Ok(ClaimOutcome::Granted { claim })
}

#[derive(Serialize, Default)]
pub struct ReleaseOutcome {
    pub released: Vec<String>,
}

/// Release `agent`'s claims. With `scope` empty, releases all of the agent's claims;
/// otherwise only claims whose scope overlaps one of the given specs.
pub fn release(root: &Path, repo: &str, agent: &str, scope: &[String]) -> Result<ReleaseOutcome> {
    let dir = claims_dir(root, repo);
    let _lock = registry_lock(root, repo)?;
    let mut released = Vec::new();
    for (path, claim) in read_claims(&dir) {
        if claim.agent != agent {
            continue;
        }
        if (scope.is_empty() || scopes_overlap(&claim.scope, scope))
            && fs::remove_file(&path).is_ok()
        {
            released.push(claim.id);
        }
    }
    Ok(ReleaseOutcome { released })
}

/// Every live claim, most recent first — the board agents read to choose work.
pub fn status(root: &Path, repo: &str) -> Vec<Claim> {
    let Ok(_lock) = registry_lock(root, repo) else {
        return Vec::new();
    };
    let mut claims = live_standalone(root, repo);
    claims.extend(crate::task::live_claims(root, repo).unwrap_or_default());
    claims.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    claims
}
