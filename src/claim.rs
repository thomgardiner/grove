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

use crate::{cache, project};

#[path = "claim_scope.rs"]
pub(crate) mod claim_scope;
use claim_scope::normalize_scope;
pub use claim_scope::resolve_scopes;

/// A claim not renewed within this long is treated as abandoned and ignored.
pub const DEFAULT_CLAIM_TTL_SECS: u64 = 30 * 60;

fn ttl_override() -> Option<u64> {
    std::env::var("GROVE_CLAIM_TTL_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
}

/// Compatibility lookup for callers that have not yet bound an operation workspace.
pub fn claim_ttl() -> u64 {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::config::Config::resolve(&workspace).claim()
}

pub fn ttl(workspace: &Path) -> u64 {
    crate::config::Config::resolve(workspace).claim()
}

fn standalone_ttl(workspace: Option<&Path>) -> Result<u64> {
    if let Some(ttl) = ttl_override() {
        return Ok(ttl);
    }
    let workspace = workspace.context(
        "standalone claim expiry requires a workspace when GROVE_CLAIM_TTL_SECS is unset",
    )?;
    Ok(crate::config::Config::resolve(workspace).claim())
}

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
    pub branch: Option<String>,
    pub created_at: u64,
}

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

impl Claim {
    fn effective_scope(&self, workspace: Option<&Path>) -> Result<Vec<String>> {
        if self.resolved_scope.is_empty() {
            let workspace =
                workspace.context("legacy claim cannot be resolved without its workspace")?;
            resolve_scopes(workspace, &self.scope)
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
    root.join("claims").join(cache::repo_slug(repo))
}

fn validate_repo(repo: &str, workspace: &Path) -> Result<()> {
    let workspace = cache::canonical_path(workspace);
    let actual = project::repo_identity(&workspace);
    if cache::canonical_path(Path::new(repo)) != cache::canonical_path(Path::new(&actual)) {
        bail!(
            "workspace {} belongs to repository {actual:?}, not {repo:?}",
            workspace.display()
        )
    }
    Ok(())
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
pub(crate) fn quarantine_corrupt(path: &Path, error: &serde_json::Error) -> Result<()> {
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

fn live_standalone(root: &Path, repo: &str, workspace: Option<&Path>) -> Result<Vec<Claim>> {
    let now = now_secs();
    let ttl = standalone_ttl(workspace)?;
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

pub(crate) fn conflicts_unlocked(
    root: &Path,
    repo: &str,
    workspace: Option<&Path>,
    scope: &[String],
    ignore_id: Option<&str>,
) -> Result<Vec<Claim>> {
    let mut conflicts = Vec::new();
    for claim in live_standalone(root, repo, workspace)?
        .into_iter()
        .chain(crate::task::live_claims(root, repo)?)
    {
        if ignore_id != Some(claim.id.as_str())
            && scopes_overlap(&claim.effective_scope(workspace)?, scope)
        {
            conflicts.push(claim);
        }
    }
    Ok(conflicts)
}

pub(crate) fn conflicts(
    root: &Path,
    repo: &str,
    workspace: &Path,
    scope: &[String],
    ignore_id: &str,
) -> Result<Vec<Claim>> {
    validate_repo(repo, workspace)?;
    let _lock = registry_lock(root, repo)?;
    conflicts_unlocked(root, repo, Some(workspace), scope, Some(ignore_id))
}

fn specs_overlap(x: &str, y: &str) -> bool {
    path_overlap(x, y)
}

pub(crate) fn path_overlap(x: &str, y: &str) -> bool {
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
    if let Some(workspace) = req.workspace {
        validate_repo(req.repo, workspace)?;
    }
    let dir = claims_dir(req.root, req.repo);
    fs::create_dir_all(&dir)?;
    // Resolve before taking the registry lock: `crate:` scopes run cargo metadata, and
    // every other agent's begin, claim, and heartbeat stalls while the lock is held.
    let resolved_scope = match req.workspace {
        Some(workspace) => resolve_scopes(workspace, &req.scope)?,
        None if req.scope.iter().any(|scope| scope.starts_with("crate:")) => {
            bail!("crate claim scopes require a workspace")
        }
        None => req
            .scope
            .iter()
            .map(|scope| normalize_scope(scope))
            .collect::<Result<Vec<_>>>()?,
    };
    let _lock = registry_lock(req.root, req.repo)?;

    let now = now_secs();
    let id = cache::repo_slug(&format!("{}|{}", req.agent, resolved_scope.join(",")));
    let conflicts = conflicts_unlocked(
        req.root,
        req.repo,
        req.workspace,
        &resolved_scope,
        Some(&id),
    )?;
    if !conflicts.is_empty() && !req.force {
        return Ok(ClaimOutcome::Conflict {
            requested: req.scope.clone(),
            conflicts,
        });
    }

    let claim = Claim {
        id,
        agent: req.agent.clone(),
        task: req.task.clone(),
        scope: req.scope.clone(),
        resolved_scope,
        branch: req.branch.clone(),
        created_at: now,
    };
    cache::write_atomic(
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
pub fn release(
    root: &Path,
    repo: &str,
    workspace: Option<&Path>,
    agent: &str,
    scope: &[String],
) -> Result<ReleaseOutcome> {
    if let Some(workspace) = workspace {
        validate_repo(repo, workspace)?;
    }
    let dir = claims_dir(root, repo);
    let _lock = registry_lock(root, repo)?;
    let scope = match workspace {
        Some(workspace) if !scope.is_empty() => resolve_scopes(workspace, scope)?,
        None if scope.iter().any(|scope| scope.starts_with("crate:")) => {
            bail!("crate claim scopes require a workspace")
        }
        _ => scope
            .iter()
            .map(|scope| normalize_scope(scope))
            .collect::<Result<Vec<_>>>()?,
    };
    let mut released = Vec::new();
    for (path, claim) in read_claims(&dir)? {
        if claim.agent != agent {
            continue;
        }
        if (scope.is_empty() || scopes_overlap(&claim.effective_scope(workspace)?, &scope))
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
pub fn status(root: &Path, repo: &str, workspace: &Path) -> Result<Vec<Claim>> {
    validate_repo(repo, workspace)?;
    let _lock = registry_lock(root, repo)?;
    let mut claims = live_standalone(root, repo, Some(workspace))?;
    claims.extend(crate::task::live_claims(root, repo).unwrap_or_default());
    claims.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    Ok(claims)
}
