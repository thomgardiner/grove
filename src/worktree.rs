//! The worktree pool: assign a fresh, prewarmed git worktree to an agent, and reap
//! the ones agents abandon.
//!
//! An agent calls `acquire` and gets a worktree on its own branch, its build lane
//! already seeded from the canonical, tracked by a lease file. When the agent dies
//! or wanders off, `reap` reclaims the worktree — salvaging any uncommitted work to
//! its branch first, so nothing is lost — and drops the lane.
//!
//! Safety: grove only ever removes a worktree it has a lease for. A human's own
//! worktree has no lease and is never touched.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, git, project};

/// A worktree idle at least this long, with no live build, is treated as abandoned.
pub const DEFAULT_REAP_TTL_SECS: u64 = 2 * 60 * 60;

/// The reap idle threshold: `GROVE_REAP_TTL_SECS` if set, else the default.
pub fn reap_ttl() -> u64 {
    std::env::var("GROVE_REAP_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(crate::config::get().reap_ttl_secs)
        .unwrap_or(DEFAULT_REAP_TTL_SECS)
}

/// The on-disk record that makes a worktree grove-managed. Its existence is what
/// authorizes reap to remove the worktree.
#[derive(Serialize, Deserialize, Clone)]
pub struct Lease {
    pub workspace: String,
    pub branch: String,
    pub agent: String,
    pub toolchain: String,
    /// The repo's shared git dir; its parent is the main worktree git commands run from.
    pub repo: String,
    pub created_at: u64,
    /// The commit the worktree branched from, so `squash` knows the fork point.
    #[serde(default)]
    pub base_oid: String,
}

/// Inputs to [`acquire`]. A request struct because the worktree location, branch, and
/// lane key all derive from these together.
pub struct AcquireRequest<'a> {
    pub root: &'a Path,
    pub cwd: &'a Path,
    pub agent: String,
    pub branch: Option<String>,
    pub base: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct RepoContext {
    /// The main worktree, which is where `git worktree add/remove` must run.
    main_root: PathBuf,
    /// Stable repo identity the canonical is keyed by (matches a build's own key).
    repo_id: String,
}

fn repo_context(dir: &Path) -> Result<RepoContext> {
    // The main worktree (first entry) is a real checkout in every repo layout, so it is
    // both a valid directory to run `git worktree ...` from and a stable anchor for
    // where grove places new worktrees — unlike the git dir's parent, which is not a
    // worktree under --separate-git-dir.
    let main_root = git::capture(dir, &["worktree", "list", "--porcelain"])?
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(|p| cache::canonical_path(Path::new(p)))
        .context("repository has no worktree")?;
    let repo_id = project::repo_identity(&main_root);
    Ok(RepoContext { main_root, repo_id })
}

/// A per-repo lock serializing git worktree operations. Concurrent `git worktree
/// add/remove` write `.git/config` under `.git/config.lock` and touch shared refs, so a
/// swarm must serialize them or hit transient lock failures. Held only around the git
/// step, never while blocking on a lane lock, so it cannot deadlock with build locks.
fn repo_git_lock(root: &Path, repo_id: &str) -> Result<File> {
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    let file = File::create(locks.join(format!("git-{}.lock", cache::repo_slug(repo_id))))
        .context("opening repo git lock")?;
    file.lock_exclusive()
        .context("locking repo git operations")?;
    Ok(file)
}

/// Where grove puts the worktrees it creates. By default they live in one central
/// directory under the grove home (`<cache-root>/worktrees/<repo>-<slug>`), namespaced
/// per repo, so agent worktrees stay in one place instead of scattering across the dev
/// folder. Override with `GROVE_WORKTREE_ROOT`.
fn worktree_root(root: &Path, repo_id: &str, main_root: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("GROVE_WORKTREE_ROOT") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = &crate::config::get().worktree_root {
        return PathBuf::from(dir);
    }
    let name = main_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    root.join("worktrees")
        .join(format!("{name}-{}", cache::repo_slug(repo_id)))
}

fn leases_dir(root: &Path) -> PathBuf {
    root.join("leases")
}

fn lease_path(root: &Path, workspace: &str, toolchain: &str) -> PathBuf {
    leases_dir(root).join(format!("{}.json", cache::lane_id(workspace, toolchain)))
}

fn leases(root: &Path) -> Vec<(PathBuf, Lease)> {
    fs::read_dir(leases_dir(root))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let lease: Lease = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
            Some((p, lease))
        })
        .collect()
}

fn find_lease(root: &Path, workspace: &str) -> Option<(PathBuf, Lease)> {
    leases(root)
        .into_iter()
        .find(|(_, lease)| lease.workspace == workspace)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn branch_exists(main_root: &Path, branch: &str) -> bool {
    git::capture(
        main_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .map(|s| !s.is_empty())
    .unwrap_or(false)
}

fn free_dir(root_dir: &Path, name: &str) -> PathBuf {
    let first = root_dir.join(name);
    if !first.exists() {
        return first;
    }
    for n in 2..=1000 {
        let candidate = root_dir.join(format!("{name}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// Choose the branch and directory for a new worktree. An explicit branch is taken
/// as-is (checked out if it already exists); an auto branch searches for a free
/// `grove/<agent>` slot so parallel agents never collide.
fn plan_slot(
    root_dir: &Path,
    main_root: &Path,
    explicit: Option<&str>,
    default_branch: &str,
) -> Result<(String, bool, PathBuf)> {
    if let Some(branch) = explicit {
        let existing = branch_exists(main_root, branch);
        return Ok((
            branch.to_string(),
            existing,
            free_dir(root_dir, &sanitize(branch)),
        ));
    }
    for n in 1..=1000 {
        let branch = if n == 1 {
            default_branch.to_string()
        } else {
            format!("{default_branch}-{n}")
        };
        let dir = root_dir.join(sanitize(&branch));
        if !dir.exists() && !branch_exists(main_root, &branch) {
            return Ok((branch, false, dir));
        }
    }
    bail!("no free worktree slot under {}", root_dir.display());
}

/// Assign an agent a fresh worktree on its own branch, its build lane prewarmed from
/// the canonical, tracked by a lease. Returns the worktree path.
pub fn acquire(req: &AcquireRequest) -> Result<PathBuf> {
    let ctx = repo_context(req.cwd)?;
    let root_dir = worktree_root(req.root, &ctx.repo_id, &ctx.main_root);
    fs::create_dir_all(&root_dir)?;

    let default_branch = format!("grove/{}", sanitize(&req.agent));
    // The commit the worktree forks from, recorded so `squash` knows where to collapse.
    let base_oid = git::capture(&ctx.main_root, &["rev-parse", &req.base]).unwrap_or_default();
    // Serialize branch/path allocation and the git worktree add per repo: two agents
    // must not pick the same slot, and concurrent `git worktree add` contends on
    // `.git/config.lock`. The lock covers only the git step, not the seed below.
    let (branch, dir) = {
        let _git = repo_git_lock(req.root, &ctx.repo_id)?;
        let (branch, existing_branch, dir) = plan_slot(
            &root_dir,
            &ctx.main_root,
            req.branch.as_deref(),
            &default_branch,
        )?;
        let dir_str = dir.to_string_lossy().into_owned();
        if existing_branch {
            git::run(&ctx.main_root, &["worktree", "add", &dir_str, &branch])?;
        } else {
            git::run(
                &ctx.main_root,
                &["worktree", "add", "-b", &branch, &dir_str, &req.base],
            )?;
        }
        (branch, dir)
    };

    let workspace = cache::canonical_path(&dir);
    let ws = workspace.to_string_lossy().into_owned();
    // Derive the toolchain from the new worktree itself: its branch or base may pin a
    // different channel than the caller's, and its lane and lease must key to that one.
    let toolchain = project::toolchain(&workspace);
    // Prewarm the new lane so the agent's first build is warm, not cold.
    let canonical = cache::canonical_dir(req.root, &ctx.repo_id, &toolchain);
    if let Some(lane) = cache::try_acquire(req.root, &ws, &toolchain)? {
        cache::seed(req.root, &lane, &canonical)?;
    }

    let lease = Lease {
        workspace: ws.clone(),
        branch,
        agent: req.agent.clone(),
        toolchain: toolchain.clone(),
        repo: ctx.repo_id,
        created_at: now_secs(),
        base_oid,
    };
    fs::create_dir_all(leases_dir(req.root))?;
    cache::write_atomic(
        &lease_path(req.root, &ws, &toolchain),
        &serde_json::to_vec_pretty(&lease)?,
    )?;
    Ok(workspace)
}

/// The repo a worktree belongs to (its canonical shared git dir), or `None` if the
/// path is not a git worktree. Compared against `lease.repo` to confirm identity.
fn worktree_repo_dir(path: &Path) -> Option<PathBuf> {
    let common = git::capture(path, &["rev-parse", "--git-common-dir"]).ok()?;
    Some(cache::canonical_path(&path.join(common)))
}

/// The branch currently checked out in `path`, or `None` if detached.
fn current_branch(path: &Path) -> Option<String> {
    git::capture(path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()
}

/// Whether the checkout at `lease.workspace` is still the one grove leased: the same
/// repo, still on the branch grove created for it. Guards against a stale lease
/// authorizing removal of a different checkout later created at the same path.
fn is_our_leased_worktree(lease: &Lease) -> bool {
    let path = Path::new(&lease.workspace);
    worktree_repo_dir(path) == Some(cache::canonical_path(Path::new(&lease.repo)))
        && current_branch(path).as_deref() == Some(lease.branch.as_str())
}

/// Preserve an abandoned worktree's uncommitted work as a durable commit before the
/// worktree is removed. Commits tracked and untracked changes; the commit lands on the
/// lease branch when the worktree is still on it, otherwise (detached HEAD, or an agent
/// that switched branches) it is pinned on a fresh `grove-salvage/…` branch so removing
/// the worktree can never leave it dangling. Ignored build artifacts are not preserved
/// — they are discarded with the worktree. Returns the ref the work landed on, if any.
fn salvage_work(worktree: &Path, lease: &Lease) -> Result<Option<String>> {
    if git::capture(worktree, &["status", "--porcelain"])?.is_empty() {
        return Ok(None);
    }
    git::run(worktree, &["add", "-A"])?;
    git::run(
        worktree,
        &[
            "commit",
            "-q",
            "-m",
            "grove: salvage work in progress before reclaim",
        ],
    )?;
    if current_branch(worktree).as_deref() == Some(lease.branch.as_str()) {
        return Ok(Some(lease.branch.clone()));
    }
    let head = git::capture(worktree, &["rev-parse", "HEAD"])?;
    let salvage = format!("grove-salvage/{}-{}", sanitize(&lease.branch), now_secs());
    git::run(worktree, &["branch", "--force", &salvage, &head])?;
    Ok(Some(salvage))
}

/// Remove a worktree without `--force`: work is salvaged first, so a plain remove
/// succeeds on the now-clean checkout, and genuinely un-salvageable state blocks
/// removal (surfaced to the caller) instead of being silently discarded.
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

/// Collapse an agent worktree branch's commits since its base into one clean commit, so
/// a swarm's stream of small commits becomes one reviewable commit. The base is
/// `base_override`, else the fork point recorded in the lease. Refuses a worktree grove
/// does not have a lease for. Uncommitted and staged changes are left exactly as-is;
/// only committed work is squashed. Built with `commit-tree` + a compare-and-swap
/// `update-ref`, so a failure at any step leaves the branch where it was — never reset
/// to its fork point with the work stranded in the reflog.
pub fn squash(
    root: &Path,
    target: &Path,
    base_override: Option<&str>,
    message: Option<&str>,
) -> Result<SquashOutcome> {
    let ws = cache::canonical_path(target);
    let ws_str = ws.to_string_lossy().into_owned();
    let (_, lease) = find_lease(root, &ws_str)
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
        // Default to the oldest commit's subject: the original intent of the work.
        None => git::capture(&ws, &["log", "--format=%s", &format!("{fork}..HEAD")])?
            .lines()
            .last()
            .unwrap_or("grove: squashed work")
            .to_string(),
    };

    // Rewriting the branch ref touches shared refs; serialize with other git ops.
    let _git = repo_git_lock(root, &lease.repo)?;
    // Build the squash commit without touching HEAD, the index, or the working tree:
    // the head tree parented on the fork. A branch whose net diff is empty squashes to
    // an empty-diff commit rather than failing halfway.
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
    // Compare-and-swap: move the ref only if it still points at the head we squashed,
    // so a racing commit is never silently discarded. The single ref move is the only
    // mutation; everything before it is scratch objects.
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
pub struct ReleaseOutcome {
    pub path: String,
    pub branch: String,
    /// The ref the worktree's uncommitted work was salvaged to, if it had any.
    pub saved_to: Option<String>,
}

/// End an agent's lease on a worktree: salvage any uncommitted work to a durable ref,
/// remove the worktree and its lane, and drop the lease. Refuses a worktree with no
/// grove lease, a checkout that is no longer the one grove leased (same guard as reap),
/// or a worktree whose lanes are mid-build — an explicit release does not trump live work.
pub fn release(root: &Path, target: &Path) -> Result<ReleaseOutcome> {
    let ws = cache::canonical_path(target).to_string_lossy().into_owned();
    let (lease_file, lease) = find_lease(root, &ws).with_context(|| {
        format!("no grove lease for {ws}; refusing to touch a worktree grove did not create")
    })?;

    let path = PathBuf::from(&ws);
    let mut saved_to = None;
    if path.exists() {
        // Same identity gate as reap: a stale lease over a path someone re-created must
        // never authorize salvage-committing their work or removing their checkout.
        if !is_our_leased_worktree(&lease) {
            bail!("{ws} is no longer grove's leased worktree; refusing to remove it");
        }
        // Refuse while any lane for this workspace is live — a build in the untagged
        // lane or a `task exec`/`verify` in a tagged one still owns the directory.
        let _lane_guard = cache::try_acquire(root, &lease.workspace, &lease.toolchain)?
            .with_context(|| format!("an active build holds {ws}; not releasing it"))?;
        let untagged = cache::lane_id(&lease.workspace, &lease.toolchain);
        if cache::workspace_busy(root, &lease.workspace, Some(&untagged)) {
            bail!("an active tagged lane holds {ws}; not releasing it");
        }
        // The main worktree is where `git worktree remove` must run. Derive it from the
        // live worktree itself: the lease's git dir has no worktree parent under
        // --separate-git-dir (see `repo_context`).
        let main_root = repo_context(&path)?.main_root;
        saved_to = salvage_work(&path, &lease)?;
        remove_worktree(&main_root, &path)?;
    }
    let _ = fs::remove_file(&lease_file);
    cache::reclaim_stale(root); // the lane's workspace is gone now, so it is reclaimed

    Ok(ReleaseOutcome {
        path: ws,
        branch: lease.branch,
        saved_to,
    })
}

#[derive(Serialize)]
pub struct WorktreeInfo {
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub agent: String,
    pub exists: bool,
    pub dirty: bool,
    pub idle_secs: u64,
    pub age_secs: u64,
}

fn activity(root: &Path, lease: &Lease) -> u64 {
    // Workspace-wide, not just the untagged build lane: an agent driving everything
    // through tagged `task exec`/`verify` lanes never touches the untagged lane, and
    // counting only that lane read a hard-working worktree as idle (and reaped it).
    lease
        .created_at
        .max(cache::workspace_last_used(root, &lease.workspace).unwrap_or(0))
}

/// Every grove-managed worktree, with its lease owner, staleness, and dirty state.
pub fn list(root: &Path) -> Vec<WorktreeInfo> {
    let now = now_secs();
    leases(root)
        .into_iter()
        .map(|(_, lease)| {
            let path = PathBuf::from(&lease.workspace);
            let exists = path.exists();
            let dirty = exists
                && git::capture(&path, &["status", "--porcelain"])
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
            WorktreeInfo {
                repo: lease.repo.clone(),
                exists,
                dirty,
                idle_secs: now.saturating_sub(activity(root, &lease)),
                age_secs: now.saturating_sub(lease.created_at),
                path: lease.workspace,
                branch: lease.branch,
                agent: lease.agent,
            }
        })
        .collect()
}

#[derive(Serialize)]
pub struct Reaped {
    pub path: String,
    pub branch: String,
    pub agent: String,
    /// The ref the worktree's uncommitted work was salvaged to, if it had any.
    pub saved_to: Option<String>,
    pub reason: String,
}

#[derive(Serialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

#[derive(Serialize, Default)]
pub struct ReapReport {
    pub reaped: Vec<Reaped>,
    pub skipped: Vec<Skipped>,
    pub dry_run: bool,
}

fn reaped_of(lease: &Lease, saved_to: Option<String>, reason: String) -> Reaped {
    Reaped {
        path: lease.workspace.clone(),
        branch: lease.branch.clone(),
        agent: lease.agent.clone(),
        saved_to,
        reason,
    }
}

/// Reclaim abandoned worktrees for the repo `cwd` belongs to. A lease whose worktree
/// is already gone is a pure orphan; a worktree idle past `ttl` with no live build is
/// abandoned — its work is salvaged to its branch before removal. `dry_run` reports
/// what would be reaped without touching anything.
pub fn reap(root: &Path, cwd: &Path, ttl: u64, dry_run: bool) -> Result<ReapReport> {
    let ctx = repo_context(cwd)?;
    let now = now_secs();
    let mut report = ReapReport {
        dry_run,
        ..Default::default()
    };

    for (lease_file, lease) in leases(root) {
        if lease.repo != ctx.repo_id {
            continue; // another repo's worktree
        }
        let path = PathBuf::from(&lease.workspace);

        if !path.exists() {
            if !dry_run {
                let _ = fs::remove_file(&lease_file);
                cache::reclaim_stale(root);
                let _ = git::run(&ctx.main_root, &["worktree", "prune"]);
                crate::events::record(
                    root,
                    &ctx.repo_id,
                    "worktree.reaped",
                    serde_json::json!({"path": lease.workspace, "branch": lease.branch, "agent": lease.agent, "reason": "orphaned"}),
                );
            }
            report
                .reaped
                .push(reaped_of(&lease, None, "orphaned".to_string()));
            continue;
        }

        let idle = now.saturating_sub(activity(root, &lease));
        if idle < ttl {
            continue; // still active
        }
        let reason = format!("idle {idle}s");

        // Never remove a checkout that is not the one we leased — a stale lease over a
        // path a human later reused. Quarantine the stale lease, leave the path alone.
        // Checked before the dry-run report too, so a dry run predicts the real outcome.
        if !is_our_leased_worktree(&lease) {
            if !dry_run {
                let _ = fs::remove_file(&lease_file);
            }
            report.skipped.push(Skipped {
                path: lease.workspace.clone(),
                reason: "path is no longer grove's leased worktree; stale lease dropped"
                    .to_string(),
            });
            continue;
        }
        if dry_run {
            report.reaped.push(reaped_of(&lease, None, reason));
            continue;
        }

        // The clock can say idle while a build is still live: a long build outruns the
        // TTL yet holds its lane lock the whole time. Take that exact lock first; if we
        // cannot, a build owns the worktree and reap must leave it alone.
        let Some(lane_guard) = cache::try_acquire(root, &lease.workspace, &lease.toolchain)? else {
            report.skipped.push(Skipped {
                path: lease.workspace.clone(),
                reason: "active build holds the lane".to_string(),
            });
            continue;
        };
        // The untagged lock alone is not enough: `task exec`, `verify`, and tagged
        // `exec` run in independent tagged lanes. If any lane for this workspace is
        // held by another process, live work owns the worktree — leave it alone.
        let untagged = cache::lane_id(&lease.workspace, &lease.toolchain);
        if cache::workspace_busy(root, &lease.workspace, Some(&untagged)) {
            report.skipped.push(Skipped {
                path: lease.workspace.clone(),
                reason: "active tagged lane holds the workspace".to_string(),
            });
            continue;
        }
        // Salvage first; if we cannot preserve the work, leave everything in place.
        match salvage_work(&path, &lease) {
            Ok(saved_to) => {
                // Remove without --force. If genuinely un-salvageable state blocks it,
                // the work is already committed — skip and leave the path in place.
                if let Err(e) = remove_worktree(&ctx.main_root, &path) {
                    report.skipped.push(Skipped {
                        path: lease.workspace.clone(),
                        reason: format!("removal blocked, left in place: {e}"),
                    });
                    continue;
                }
                let _ = fs::remove_file(&lease_file);
                drop(lane_guard); // release before reclaim so the lane dir can be removed
                cache::reclaim_stale(root);
                crate::events::record(
                    root,
                    &ctx.repo_id,
                    "worktree.reaped",
                    serde_json::json!({"path": lease.workspace, "branch": lease.branch, "agent": lease.agent, "reason": &reason, "saved_to": &saved_to}),
                );
                report.reaped.push(reaped_of(&lease, saved_to, reason));
            }
            Err(e) => report.skipped.push(Skipped {
                path: lease.workspace.clone(),
                reason: format!("could not salvage work, left in place: {e}"),
            }),
        }
    }
    Ok(report)
}
