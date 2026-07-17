//! Work coordination (opt-in). Agents declare what part of the repo they are working on
//! so a swarm can pick non-overlapping work and not clobber each other. A claim is
//! grove's lease model — an exclusive flock plus a TTL — pointed at a scope of the repo
//! (paths or crates) instead of a worktree.
//!
//! `claim` is first-wins: it takes the registry lock, rejects a scope that overlaps a
//! live claim from another agent, and otherwise records the claim. `status` is the board
//! agents read to decide what to work on. Claims expire on a TTL, like abandoned
//! worktrees, so a dead agent never holds work forever.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::scope::normalize as normalize_scope;

/// A claim not renewed within this long is treated as abandoned and ignored.
pub const DEFAULT_CLAIM_TTL_SECS: u64 = 30 * 60;

/// One agent's declared work: a scope of the repo it is actively changing. Scope entries
/// are repo-relative path prefixes, or `crate:<name>` for a whole crate.
#[derive(Serialize, Deserialize, Clone)]
pub struct Claim {
    pub id: String,
    pub agent: String,
    pub task: String,
    pub scope: Vec<String>,
    /// Canonical path scopes used for overlap checks; preserves the requested scope
    /// above for humans while making `crate:name` and paths one namespace.
    #[serde(default)]
    pub resolved_scope: Vec<String>,
    /// Claims sharing a group deliberately overlap without conflicting:
    /// N-version attempts at one order, where only one result will land.
    /// Outsiders still conflict with every member.
    #[serde(default)]
    pub group: Option<String>,
    pub branch: Option<String>,
    pub created_at: u64,
}

pub struct ClaimRequest<'a> {
    pub root: &'a Path,
    pub repo: &'a str,
    pub ttl: u64,
    pub agent: String,
    pub task: String,
    pub scope: Vec<String>,
    /// Original user-facing scope text. Adapters set this when they expanded
    /// ecosystem selectors before handing normalized scopes to core.
    pub requested_scope: Option<Vec<String>>,
    pub branch: Option<String>,
    pub force: bool,
}

impl Claim {
    fn effective_scope(&self) -> Result<Vec<String>> {
        if self.resolved_scope.is_empty() {
            self.scope
                .iter()
                .map(|scope| {
                    if scope.starts_with("crate:") {
                        bail!("legacy claim requires an adapter to resolve its scope")
                    }
                    normalize_scope(scope)
                })
                .collect()
        } else {
            Ok(self.resolved_scope.clone())
        }
    }
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
    root.join("claims").join(crate::repo_slug(repo))
}

/// Serialize claim/release so the overlap check and the write are one atomic step; two
/// agents racing for overlapping scopes then resolve to exactly one winner.
pub fn registry_lock(root: &Path, repo: &str) -> Result<File> {
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    let file = File::create(locks.join(format!("claims-{}.lock", crate::repo_slug(repo))))
        .context("opening claim-registry lock")?;
    file.lock_exclusive().context("locking claim registry")?;
    Ok(file)
}

fn read_claims(dir: &Path) -> Result<Vec<(PathBuf, Claim)>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut claims = Vec::new();
    for entry in entries {
        let path = entry.context("reading claim directory")?.path();
        if !path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        match serde_json::from_slice(&bytes) {
            Ok(claim) => claims.push((path, claim)),
            Err(error) => quarantine_corrupt(&path, &error)?,
        }
    }
    Ok(claims)
}

/// Move an unparseable registry record aside so one corrupt file cannot halt every
/// claim and task operation in the repository. The bytes stay beside the original as
/// `<name>.corrupt` for inspection. A record another process already moved is skipped;
/// any other rename failure keeps the original fail-closed behavior.
pub fn quarantine_corrupt(path: &Path, error: &serde_json::Error) -> Result<()> {
    let mut name = path
        .file_name()
        .context("corrupt record has no file name")?
        .to_os_string();
    name.push(".corrupt");
    let target = path.with_file_name(name);
    match fs::rename(path, &target) {
        Ok(()) => {
            eprintln!(
                "grove: quarantined corrupt record {} as {}: {error}; review live work with grove status",
                path.display(),
                target.display()
            );
            Ok(())
        }
        Err(rename_error) if rename_error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(rename_error) => Err(rename_error)
            .with_context(|| format!("parsing {}: {error}; quarantine failed", path.display())),
    }
}

/// Two resolved path scopes overlap if any of their entries do.
fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.iter().any(|y| specs_overlap(x, y)))
}

fn live_standalone(root: &Path, repo: &str, ttl: u64) -> Result<Vec<Claim>> {
    let now = now_secs();
    let dir = claims_dir(root, repo);
    Ok(read_claims(&dir)?
        .into_iter()
        .filter_map(|(path, claim)| {
            if now.saturating_sub(claim.created_at) <= ttl {
                Some(claim)
            } else {
                let _ = fs::remove_file(path);
                None
            }
        })
        .collect())
}

pub fn conflicts_unlocked(
    root: &Path,
    repo: &str,
    ttl: u64,
    scope: &[String],
    ignore_id: Option<&str>,
    group: Option<&str>,
) -> Result<Vec<Claim>> {
    let mut conflicts = Vec::new();
    for claim in live_standalone(root, repo, ttl)?.into_iter() {
        let shared_group = group.is_some() && claim.group.as_deref() == group;
        if ignore_id != Some(claim.id.as_str())
            && !shared_group
            && scopes_overlap(&claim.effective_scope()?, scope)
        {
            conflicts.push(claim);
        }
    }
    Ok(conflicts)
}

pub fn conflicts(
    root: &Path,
    repo: &str,
    ttl: u64,
    scope: &[String],
    ignore_id: &str,
) -> Result<Vec<Claim>> {
    let _lock = registry_lock(root, repo)?;
    conflicts_unlocked(root, repo, ttl, scope, Some(ignore_id), None)
}

fn specs_overlap(x: &str, y: &str) -> bool {
    path_overlap(x, y)
}

pub fn path_overlap(x: &str, y: &str) -> bool {
    let x = x.trim_matches('/');
    let y = y.trim_matches('/');
    x.is_empty()
        || y.is_empty()
        || x == "."
        || y == "."
        || x == y
        || y.starts_with(&format!("{x}/"))
        || x.starts_with(&format!("{y}/"))
}

/// Claim a scope of the repo for `agent`. First-wins: rejects a scope overlapping another
/// agent's live claim unless `force`. Renewing the same agent's overlapping claim is fine.
pub fn claim(req: &ClaimRequest) -> Result<ClaimOutcome> {
    let dir = claims_dir(req.root, req.repo);
    fs::create_dir_all(&dir)?;
    let resolved_scope = req.scope.clone();
    let _lock = registry_lock(req.root, req.repo)?;

    let now = now_secs();
    let id = crate::repo_slug(&format!("{}|{}", req.agent, resolved_scope.join(",")));
    let conflicts = conflicts_unlocked(
        req.root,
        req.repo,
        req.ttl,
        &resolved_scope,
        Some(&id),
        None,
    )?;
    if !conflicts.is_empty() && !req.force {
        return Ok(ClaimOutcome::Conflict {
            requested: req
                .requested_scope
                .clone()
                .unwrap_or_else(|| req.scope.clone()),
            conflicts,
        });
    }

    let claim = Claim {
        id,
        agent: req.agent.clone(),
        task: req.task.clone(),
        scope: req
            .requested_scope
            .clone()
            .unwrap_or_else(|| req.scope.clone()),
        resolved_scope,
        group: None,
        branch: req.branch.clone(),
        created_at: now,
    };
    crate::write_atomic(
        &dir.join(format!("{}.json", claim.id)),
        &serde_json::to_vec_pretty(&claim)?,
    )?;
    crate::events::record(
        req.root,
        req.repo,
        "claim.granted",
        serde_json::json!({"agent": claim.agent, "id": claim.id, "scope": claim.resolved_scope}),
    );
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
    for (path, claim) in read_claims(&dir)? {
        if claim.agent != agent {
            continue;
        }
        if (scope.is_empty() || scopes_overlap(&claim.effective_scope()?, scope))
            && fs::remove_file(&path).is_ok()
        {
            released.push(claim.id);
        }
    }
    if !released.is_empty() {
        crate::events::record(
            root,
            repo,
            "claim.released",
            serde_json::json!({"agent": agent, "released": released}),
        );
    }
    Ok(ReleaseOutcome { released })
}

/// Every live claim, most recent first — the board agents read to choose work.
pub fn status(root: &Path, repo: &str, ttl: u64) -> Result<Vec<Claim>> {
    let _lock = registry_lock(root, repo)?;
    let mut claims = live_standalone(root, repo, ttl)?;
    claims.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    Ok(claims)
}
