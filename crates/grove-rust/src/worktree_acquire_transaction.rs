use super::*;
use crate::worktree::worktree_lease::generation;
use crate::worktree::worktree_materialization::{Add, MaterializationRecord, add, head};

pub(super) struct Slot {
    pub(super) branch: String,
    pub(super) existing: bool,
    pub(super) workspace: PathBuf,
    pub(super) base_oid: String,
}

pub(super) fn slot(
    request: &AcquireRequest<'_>,
    ctx: &RepoContext,
    root_dir: &Path,
    default_branch: &str,
) -> Result<Slot> {
    let (branch, existing, workspace) = plan_slot(
        root_dir,
        &ctx.main_root,
        request.branch.as_deref(),
        default_branch,
    )?;
    let base = if existing {
        branch.as_str()
    } else {
        request.base.as_str()
    };
    let base_oid =
        git::capture(&ctx.main_root, &["rev-parse", base]).context("resolving worktree base")?;
    Ok(Slot {
        branch,
        existing,
        workspace,
        base_oid,
    })
}

pub(super) fn same(left: &Slot, right: &Slot) -> bool {
    left.branch == right.branch
        && left.existing == right.existing
        && left.workspace == right.workspace
        && left.base_oid == right.base_oid
}

pub(super) fn recovered(request: &AcquireRequest<'_>, ctx: &RepoContext) -> Result<Option<Lease>> {
    let base = request
        .branch
        .as_deref()
        .filter(|branch| branch_exists(&ctx.main_root, branch))
        .unwrap_or(&request.base);
    let base_oid =
        git::capture(&ctx.main_root, &["rev-parse", base]).context("resolving worktree base")?;
    Ok(reconcile(request.root, ctx)?
        .into_iter()
        .find(|lease| recovered_for(request, &base_oid, lease)))
}

pub(super) fn intent(
    request: &AcquireRequest<'_>,
    ctx: &RepoContext,
    slot: &Slot,
    materialization: Option<materialized::State>,
) -> AcquisitionIntent {
    AcquisitionIntent {
        repo: ctx.repo_id.clone(),
        main_worktree: ctx.main_root.to_string_lossy().into_owned(),
        workspace: slot.workspace.to_string_lossy().into_owned(),
        branch: slot.branch.clone(),
        agent: request.agent.clone(),
        base_oid: slot.base_oid.clone(),
        created_at: now_secs(),
        materialization,
    }
}

pub(super) fn exact(ctx: &RepoContext, intent: &AcquisitionIntent) -> Result<()> {
    if !exact_worktree(ctx, intent)
        || head(Path::new(&intent.workspace)).map_err(anyhow::Error::new)? != intent.base_oid
    {
        bail!(
            "git worktree add did not create the planned repository, branch, and base at {}",
            intent.workspace
        )
    }
    Ok(())
}

pub(super) fn publish(
    root: &Path,
    intent_file: &Path,
    intent: &AcquisitionIntent,
    materialization: Option<MaterializationRecord>,
) -> Result<Lease> {
    let lease = Lease {
        workspace: intent.workspace.clone(),
        branch: intent.branch.clone(),
        agent: intent.agent.clone(),
        toolchain: project::toolchain(Path::new(&intent.workspace)),
        repo: intent.repo.clone(),
        created_at: intent.created_at,
        generation: generation(),
        last_activity: 0,
        base_oid: intent.base_oid.clone(),
        materialization,
    };
    write_lease(root, &lease)?;
    remove_durable_intent(intent_file);
    Ok(lease)
}

fn git_add(
    ctx: &RepoContext,
    branch: &str,
    existing: bool,
    workspace: &Path,
    base: &str,
) -> Result<()> {
    add(&Add {
        main: &ctx.main_root,
        branch,
        existing,
        workspace,
        base,
        checkout: true,
    })
    .map_err(anyhow::Error::new)
}

pub(super) fn acquire_with<F>(
    request: &AcquireRequest<'_>,
    config: Option<&config::Config>,
    add_worktree: F,
) -> Result<PathBuf>
where
    F: FnOnce(&RepoContext, &str, bool, &Path, &str) -> Result<()>,
{
    let ctx = repo_context(request.cwd)?;
    let owned = config
        .is_none()
        .then(|| config::Config::resolve(&ctx.main_root));
    let config = config
        .or(owned.as_ref())
        .context("resolving configuration")?;
    if let Some(lease) = recovered(request, &ctx)? {
        seed(request.root, &lease);
        return Ok(PathBuf::from(lease.workspace));
    }
    let root_dir = worktree_root(config, request.root, &ctx.repo_id, &ctx.main_root);
    fs::create_dir_all(&root_dir)?;
    let root_dir = cache::canonical_path(&root_dir);
    let default_branch = format!("grove/{}", sanitize(&request.agent));
    let mut add_worktree = Some(add_worktree);
    let lease = loop {
        let planned = {
            let _git = repo_git_lock(request.root, &ctx.repo_id)?;
            slot(request, &ctx, &root_dir, &default_branch)?
        };
        let _lifecycle =
            cache::lifecycle_exclusive(request.root, &ctx.repo_id, &planned.workspace)?;
        let _git = repo_git_lock(request.root, &ctx.repo_id)?;
        let current = slot(request, &ctx, &root_dir, &default_branch)?;
        if !same(&planned, &current) {
            continue;
        }
        let intent = intent(request, &ctx, &current, None);
        let intent_file = write_intent(request.root, &intent)?;
        add_worktree
            .take()
            .context("worktree slot changed after Git callback")?(
            &ctx,
            &intent.branch,
            current.existing,
            &current.workspace,
            &intent.base_oid,
        )?;
        exact(&ctx, &intent)?;
        break publish(request.root, &intent_file, &intent, None)?;
    };
    seed(request.root, &lease);
    Ok(PathBuf::from(lease.workspace))
}

pub fn acquire(request: &AcquireRequest<'_>) -> Result<PathBuf> {
    acquire_with(request, None, git_add)
}

pub fn bind(request: &AcquireRequest<'_>, config: &config::Config) -> Result<PathBuf> {
    acquire_with(request, Some(config), git_add)
}
