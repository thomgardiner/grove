use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::claim::claim_scope::normalize_scope;
use crate::{cache, config};

use super::worktree_materialization::{
    FallbackReason, Fingerprint, MaterializationIntent, MaterializationMode, MaterializationPlan,
    MaterializationRecord, PlanInput, SCHEMA_VERSION, capture, equivalent, expand as plan_expand,
    fingerprint, measure,
};
use super::{
    Lease, containing, find_lease, is_our_leased_worktree, now_secs, repo_git_lock, write_lease,
};

enum Request<'a> {
    Expand(&'a [String]),
    Full,
}

#[path = "worktree_materialize_apply.rs"]
mod apply;
use apply::reconcile;

struct Locked {
    lease: Lease,
    workspace: PathBuf,
    cargo: PathBuf,
    intent: PathBuf,
}

/// Expand a managed sparse worktree for additional package or path scopes.
/// Unmanaged and already-full worktrees are left unchanged.
pub fn expand(
    root: &Path,
    target: &Path,
    scopes: &[String],
) -> Result<Option<MaterializationRecord>> {
    if scopes.is_empty() {
        bail!("materialization expansion requires at least one scope")
    }
    change(root, target, Request::Expand(scopes))
}

/// Convert a managed sparse worktree to a normal full checkout.
/// Unmanaged and already-full worktrees are left unchanged.
pub fn full(root: &Path, target: &Path) -> Result<Option<MaterializationRecord>> {
    change(root, target, Request::Full)
}

fn change(
    root: &Path,
    target: &Path,
    request: Request<'_>,
) -> Result<Option<MaterializationRecord>> {
    let Some((_, initial)) = containing(root, target)? else {
        return Ok(None);
    };
    let workspace = PathBuf::from(&initial.workspace);
    let _lifecycle = cache::lifecycle_exclusive(root, &initial.repo, &workspace)?;
    let _git = repo_git_lock(root, &initial.repo)?;
    let state = locked(root, target, &initial)?;
    if let Some(intent) = read(&state.intent)? {
        reconcile(root, &state, intent)?;
    }
    let locked = locked(root, target, &initial)?;
    let Some(prior) = locked.lease.materialization.clone() else {
        return Ok(None);
    };
    prior.validate()?;
    if prior.mode == MaterializationMode::Full {
        return Ok(Some(prior));
    }
    let intent = intent(&locked, prior, request)?;
    intent.validate()?;
    cache::write_atomic(&locked.intent, &serde_json::to_vec_pretty(&intent)?)?;
    reconcile(root, &locked, intent).map(Some)
}

fn locked(root: &Path, target: &Path, initial: &Lease) -> Result<Locked> {
    let workspace = cache::canonical_path(Path::new(&initial.workspace));
    let (_, lease) = find_lease(root, &initial.workspace)?
        .context("managed worktree lease disappeared during materialization")?;
    if lease.repo != initial.repo || !is_our_leased_worktree(&lease) {
        bail!("managed worktree identity changed during materialization")
    }
    let target = cache::canonical_path(target);
    let cargo = if target.starts_with(&workspace) {
        target
    } else {
        bail!("materialization target moved outside its managed worktree")
    };
    Ok(Locked {
        intent: path(root, &lease),
        lease,
        workspace,
        cargo,
    })
}

fn path(root: &Path, lease: &Lease) -> PathBuf {
    root.join("materializations")
        .join(cache::repo_slug(&lease.repo))
        .join(format!(
            "{}.json",
            cache::lane_id(&lease.workspace, &lease.toolchain)
        ))
}

pub(super) fn cleanup_ready(root: &Path, lease: &Lease) -> Result<()> {
    if let Some(record) = &lease.materialization {
        record
            .validate()
            .context("managed worktree has a contradictory materialization record")?;
    }
    let intent = path(root, lease);
    if intent.try_exists()? {
        bail!(
            "unresolved materialization intent {}; refusing cleanup",
            intent.display()
        )
    }
    Ok(())
}

fn read(path: &Path) -> Result<Option<MaterializationIntent>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| {
                format!(
                    "parsing materialization intent {}; preserved for inspection",
                    path.display()
                )
            })
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn intent(
    locked: &Locked,
    prior: MaterializationRecord,
    request: Request<'_>,
) -> Result<MaterializationIntent> {
    let cargo_dir = relative(&locked.workspace, &locked.cargo)?;
    let (plan, requested, closure, support, mode, desired) = match request {
        Request::Full => (
            None,
            prior.requested_scopes.clone(),
            prior.closure_cones.clone(),
            prior.support_cones.clone(),
            MaterializationMode::Full,
            Vec::new(),
        ),
        Request::Expand(scopes) => expansion(locked, &prior, scopes)?,
    };
    Ok(MaterializationIntent {
        schema_version: SCHEMA_VERSION,
        repo: locked.lease.repo.clone(),
        workspace: locked.lease.workspace.clone(),
        cargo_dir,
        branch: locked.lease.branch.clone(),
        base_oid: locked.lease.base_oid.clone(),
        prior,
        desired_mode: mode,
        requested_scopes: requested,
        closure_cones: closure,
        support_cones: support,
        current_cones: locked
            .lease
            .materialization
            .as_ref()
            .context("sparse lease lost its materialization record")?
            .current_cones
            .clone(),
        desired_cones: desired,
        plan,
        created_at: now_secs(),
    })
}

type Desired = (
    Option<MaterializationPlan>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    MaterializationMode,
    Vec<String>,
);

fn expansion(locked: &Locked, prior: &MaterializationRecord, scopes: &[String]) -> Result<Desired> {
    let mut requested: BTreeSet<_> = prior.requested_scopes.iter().cloned().collect();
    for scope in scopes {
        requested.insert(normalize(scope)?);
    }
    let requested: Vec<_> = requested.into_iter().collect();
    let source = capture(&locked.cargo, &locked.workspace)?;
    if prior.source_cargo_fingerprint.as_deref() != Some(fingerprint(&source)) {
        return Ok(full_desired(prior, requested));
    }
    let config = config::Config::resolve(&locked.cargo);
    let mut plan = selection(locked, &config, &requested, &source)?;
    full_metrics(&mut plan, prior)?;
    let after = capture(&locked.cargo, &locked.workspace)?;
    if !equivalent(&source, &after)? {
        return Ok(full_desired(prior, requested));
    }
    let mode = plan.mode;
    let desired = if mode == MaterializationMode::Sparse {
        union(
            &prior.current_cones,
            plan.closure_cones
                .iter()
                .chain(&plan.support_cones)
                .cloned(),
        )
    } else {
        Vec::new()
    };
    Ok((
        Some(plan.clone()),
        plan.requested_scopes.clone(),
        plan.closure_cones.clone(),
        plan.support_cones.clone(),
        mode,
        desired,
    ))
}

fn selection(
    locked: &Locked,
    config: &config::Config,
    scopes: &[String],
    source: &Fingerprint,
) -> Result<MaterializationPlan> {
    let repository = config::Config::repository(&locked.cargo);
    plan_expand(PlanInput {
        workspace: &locked.cargo,
        base_oid: &locked.lease.base_oid,
        scopes,
        extras: &config.materialize()?,
        config: repository.as_deref(),
        fingerprint: source,
        planned_at: now_secs(),
    })
}

fn full_metrics(plan: &mut MaterializationPlan, prior: &MaterializationRecord) -> Result<()> {
    plan.full_working_files = prior.full_working_files.max(plan.selected_working_files);
    plan.full_working_logical_bytes = prior
        .full_working_logical_bytes
        .max(plan.selected_working_logical_bytes);
    plan.validate()
}

fn full_desired(prior: &MaterializationRecord, requested: Vec<String>) -> Desired {
    (
        None,
        requested,
        prior.closure_cones.clone(),
        prior.support_cones.clone(),
        MaterializationMode::Full,
        Vec::new(),
    )
}

fn normalize(scope: &str) -> Result<String> {
    if let Some(name) = scope.strip_prefix("crate:") {
        if name.is_empty() {
            bail!("crate materialization scope requires a name")
        }
        return Ok(format!("crate:{name}"));
    }
    normalize_scope(scope)
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    let path = path
        .strip_prefix(root)
        .context("Cargo workspace is outside its managed worktree")?;
    Ok(if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().replace('\\', "/")
    })
}

fn union(current: &[String], desired: impl IntoIterator<Item = String>) -> Vec<String> {
    current
        .iter()
        .cloned()
        .chain(desired)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
