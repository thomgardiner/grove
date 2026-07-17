use super::worktree_lease::{find_lease, generation, write_lease};
use super::{Lease, now_secs};
use crate::{cache, config, git, project};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
pub(super) struct RepoContext {
    pub(super) main_root: PathBuf,
    pub(super) repo_id: String,
}
pub(super) fn repo_context(dir: &Path) -> Result<RepoContext> {
    let main_root = git::capture(dir, &["worktree", "list", "--porcelain"])?
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(|path| cache::canonical_path(Path::new(path)))
        .context("repository has no worktree")?;
    let repo_id = project::repo_identity(&main_root);
    Ok(RepoContext { main_root, repo_id })
}
fn worktree_repo_dir(path: &Path) -> Option<PathBuf> {
    let common = git::capture(path, &["rev-parse", "--git-common-dir"]).ok()?;
    Some(cache::canonical_path(&path.join(common)))
}
pub(super) fn current_branch(path: &Path) -> Option<String> {
    git::capture(path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()
}
pub(super) fn is_our_leased_worktree(lease: &Lease) -> bool {
    let path = Path::new(&lease.workspace);
    worktree_repo_dir(path) == Some(cache::canonical_path(Path::new(&lease.repo)))
        && current_branch(path).as_deref() == Some(lease.branch.as_str())
}
pub(super) fn repo_git_lock(root: &Path, repo_id: &str) -> Result<File> {
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    let file = File::create(locks.join(format!("git-{}.lock", cache::repo_slug(repo_id))))
        .context("opening repo git lock")?;
    file.lock_exclusive()
        .context("locking repo git operations")?;
    Ok(file)
}
pub(super) fn worktree_root(
    config: &config::Config,
    root: &Path,
    repo_id: &str,
    main_root: &Path,
) -> PathBuf {
    if let Some(path) = config.worktrees() {
        return if path.is_absolute() {
            path
        } else {
            main_root.join(path)
        };
    }
    let name = main_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    root.join("worktrees")
        .join(format!("{name}-{}", cache::repo_slug(repo_id)))
}
pub(super) fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect()
}
fn branch_exists(main_root: &Path, branch: &str) -> bool {
    let reference = format!("refs/heads/{branch}");
    git::capture(main_root, &["rev-parse", "--verify", "--quiet", &reference])
        .is_ok_and(|output| !output.is_empty())
}
fn free_dir(root: &Path, name: &str) -> PathBuf {
    let first = root.join(name);
    if !first.exists() {
        return first;
    }
    (2..=1000)
        .map(|number| root.join(format!("{name}-{number}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

fn plan_slot(
    root: &Path,
    main_root: &Path,
    explicit: Option<&str>,
    default_branch: &str,
) -> Result<(String, bool, PathBuf)> {
    if let Some(branch) = explicit {
        return Ok((
            branch.to_string(),
            branch_exists(main_root, branch),
            free_dir(root, &sanitize(branch)),
        ));
    }
    for number in 1..=1000 {
        let branch = if number == 1 {
            default_branch.to_string()
        } else {
            format!("{default_branch}-{number}")
        };
        let path = root.join(sanitize(&branch));
        if !path.exists() && !branch_exists(main_root, &branch) {
            return Ok((branch, false, path));
        }
    }
    bail!("no free worktree slot under {}", root.display())
}

#[derive(Clone, Deserialize, Serialize)]
struct AcquisitionIntent {
    repo: String,
    main_worktree: String,
    workspace: String,
    branch: String,
    agent: String,
    base_oid: String,
    created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    materialization: Option<materialized::State>,
}

fn intents_dir(root: &Path, repo: &str) -> PathBuf {
    root.join("acquisitions").join(cache::repo_slug(repo))
}

fn workspace_intent_path(root: &Path, repo: &str, workspace: &str) -> PathBuf {
    intents_dir(root, repo).join(format!("{}.json", cache::repo_slug(workspace)))
}

fn intent_path(root: &Path, intent: &AcquisitionIntent) -> PathBuf {
    workspace_intent_path(root, &intent.repo, &intent.workspace)
}

pub(super) fn cleanup_ready(root: &Path, lease: &Lease) -> Result<()> {
    let intent = workspace_intent_path(root, &lease.repo, &lease.workspace);
    if intent.try_exists()? {
        bail!(
            "unresolved acquisition intent {}; refusing cleanup",
            intent.display()
        )
    }
    Ok(())
}

fn json_entry(entry: &fs::DirEntry) -> bool {
    entry.path().extension() == Some(std::ffi::OsStr::new("json"))
}
fn read_intents(root: &Path, repo: &str) -> Result<Vec<(PathBuf, AcquisitionIntent)>> {
    let Ok(entries) = fs::read_dir(intents_dir(root, repo)) else {
        return Ok(Vec::new());
    };
    entries
        .filter(|entry| entry.as_ref().map_or(true, json_entry))
        .map(|entry| {
            let path = entry?.path();
            let bytes = fs::read(&path)?;
            let intent = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "parsing acquisition intent {}; preserved for inspection",
                    path.display()
                )
            })?;
            Ok((path, intent))
        })
        .collect()
}
fn write_intent(root: &Path, intent: &AcquisitionIntent) -> Result<PathBuf> {
    let path = intent_path(root, intent);
    fs::create_dir_all(path.parent().context("acquisition intent has no parent")?)?;
    cache::write_atomic(&path, &serde_json::to_vec_pretty(intent)?)?;
    Ok(path)
}
fn exact_worktree(ctx: &RepoContext, intent: &AcquisitionIntent) -> bool {
    let workspace = Path::new(&intent.workspace);
    intent.repo == ctx.repo_id
        && cache::canonical_path(Path::new(&intent.main_worktree)) == ctx.main_root
        && cache::canonical_path(workspace) == workspace
        && worktree_repo_dir(workspace) == Some(cache::canonical_path(Path::new(&intent.repo)))
        && current_branch(workspace).as_deref() == Some(intent.branch.as_str())
        && git::capture(workspace, &["rev-parse", "HEAD"]).is_ok_and(|head| head == intent.base_oid)
}

fn lease_matches_intent(lease: &Lease, intent: &AcquisitionIntent) -> bool {
    lease.workspace == intent.workspace
        && lease.repo == intent.repo
        && lease.branch == intent.branch
        && lease.agent == intent.agent
        && lease.base_oid == intent.base_oid
}
fn remove_durable_intent(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        eprintln!(
            "grove: lease is durable but acquisition intent {} remains: {error}",
            path.display()
        );
    }
}

pub(super) fn reconcile(root: &Path, ctx: &RepoContext) -> Result<Vec<Lease>> {
    let mut adopted = Vec::new();
    for (path, snapshot) in read_intents(root, &ctx.repo_id)? {
        if snapshot.repo != ctx.repo_id
            || cache::canonical_path(Path::new(&snapshot.main_worktree)) != ctx.main_root
        {
            eprintln!(
                "grove: preserving identity-mismatched intent {}",
                path.display()
            );
            continue;
        }
        let _lifecycle =
            cache::lifecycle_exclusive(root, &snapshot.repo, Path::new(&snapshot.workspace))?;
        let _git = repo_git_lock(root, &snapshot.repo)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let intent: AcquisitionIntent = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "parsing acquisition intent {}; preserved for inspection",
                path.display()
            )
        })?;
        let workspace = Path::new(&intent.workspace);
        if intent.repo != ctx.repo_id
            || cache::canonical_path(Path::new(&intent.main_worktree)) != ctx.main_root
        {
            eprintln!(
                "grove: preserving identity-mismatched intent {}",
                path.display()
            );
            continue;
        }
        if !workspace.exists() {
            fs::remove_file(&path)?;
            eprintln!(
                "grove: cleared pre-Git intent {}; branch {} was not deleted",
                path.display(),
                intent.branch
            );
            continue;
        }
        if !exact_worktree(ctx, &intent) {
            eprintln!(
                "grove: preserving intent {}: workspace identity or branch does not match",
                path.display()
            );
            continue;
        }
        if let Some((_, lease)) = find_lease(root, &intent.workspace)? {
            if lease_matches_intent(&lease, &intent) {
                remove_durable_intent(&path);
                adopted.push(lease);
            } else {
                eprintln!(
                    "grove: preserving intent {}: an incompatible lease already exists",
                    path.display()
                );
            }
            continue;
        }
        if intent.materialization.is_some() {
            let lease = materialized::recover(root, ctx, &path, &intent)?;
            adopted.push(lease);
            continue;
        }
        let lease = Lease {
            workspace: intent.workspace.clone(),
            branch: intent.branch.clone(),
            agent: intent.agent.clone(),
            toolchain: project::toolchain(workspace),
            repo: intent.repo.clone(),
            created_at: intent.created_at,
            generation: generation(),
            last_activity: now_secs(),
            base_oid: intent.base_oid.clone(),
            materialization: None,
        };
        write_lease(root, &lease)?;
        remove_durable_intent(&path);
        eprintln!(
            "grove: adopted interrupted worktree {} on {}",
            intent.workspace, intent.branch
        );
        adopted.push(lease);
    }
    Ok(adopted)
}

pub struct AcquireRequest<'a> {
    pub root: &'a Path,
    pub cwd: &'a Path,
    pub agent: String,
    pub branch: Option<String>,
    pub base: String,
}

fn recovered_for(request: &AcquireRequest<'_>, base_oid: &str, lease: &Lease) -> bool {
    lease.agent == request.agent
        && lease.base_oid == base_oid
        && request
            .branch
            .as_ref()
            .is_none_or(|branch| branch == &lease.branch)
}

fn seed(root: &Path, lease: &Lease) {
    let result = (|| -> Result<()> {
        let canonical = cache::canonical_dir(root, &lease.repo, &lease.toolchain);
        if let Some(lane) = cache::try_acquire(root, &lease.workspace, &lease.toolchain)? {
            cache::seed(root, &lane, &canonical)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!(
            "grove: worktree {} is safely leased, but lane prewarm failed: {error:#}",
            lease.workspace
        );
    }
}

#[path = "worktree_acquire_transaction.rs"]
mod transaction;
#[cfg(test)]
use transaction::acquire_with;
pub use transaction::{acquire, bind};
#[path = "worktree_acquire_materialized.rs"]
mod materialized;
#[doc(hidden)]
pub fn scoped(
    request: &AcquireRequest<'_>,
    scopes: &[String],
    config: &config::Config,
) -> Result<PathBuf> {
    materialized::scoped(request, scopes, config)
}

#[cfg(test)]
#[path = "worktree_acquire_tests.rs"]
mod tests;
