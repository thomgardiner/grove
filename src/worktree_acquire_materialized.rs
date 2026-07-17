use super::transaction::{exact, intent, publish, recovered, same, slot};
use super::*;
use crate::worktree::worktree_materialization::{
    Add, Failure, FallbackReason, Fingerprint, MaterializationMode, MaterializationPlan,
    MaterializationRecord, PlanInput, add, capture, equivalent, fingerprint, full, plan, sparse,
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct State {
    scopes: Vec<String>,
    extras: Vec<String>,
    cargo_dir: String,
    config: Option<String>,
    plan: Option<MaterializationPlan>,
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    let path = cache::canonical_path(path);
    let path = path
        .strip_prefix(root)
        .context("Cargo directory is outside the repository")?;
    Ok(if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().replace('\\', "/")
    })
}

fn inside(workspace: &Path, relative: &str) -> PathBuf {
    if relative == "." {
        workspace.to_path_buf()
    } else {
        workspace.join(relative)
    }
}

fn state(
    request: &AcquireRequest<'_>,
    config: &config::Config,
    scopes: &[String],
) -> Result<State> {
    let source = PathBuf::from(git::capture(
        request.cwd,
        &["rev-parse", "--show-toplevel"],
    )?);
    let source = cache::canonical_path(&source);
    let config_path = config::Config::repository(request.cwd)
        .map(|path| relative(&source, &path))
        .transpose()?;
    Ok(State {
        scopes: scopes.to_vec(),
        extras: config.materialize()?,
        cargo_dir: relative(&source, request.cwd)?,
        config: config_path,
        plan: None,
    })
}

fn planned(
    intent: &mut AcquisitionIntent,
    intent_file: &Path,
    workspace: &Path,
) -> Result<(Fingerprint, MaterializationPlan)> {
    let state = intent
        .materialization
        .as_ref()
        .context("scoped acquisition omitted its durable state")?;
    let cargo = inside(workspace, &state.cargo_dir);
    let config = state.config.as_deref().map(|path| inside(workspace, path));
    let source = capture(&cargo, workspace)?;
    let plan = plan(PlanInput {
        workspace: &cargo,
        base_oid: &intent.base_oid,
        scopes: &state.scopes,
        extras: &state.extras,
        config: config.as_deref(),
        fingerprint: &source,
        planned_at: now_secs(),
    })?;
    intent.materialization.as_mut().unwrap().plan = Some(plan.clone());
    cache::write_atomic(intent_file, &serde_json::to_vec_pretty(intent)?)?;
    Ok((source, plan))
}

fn preplan(
    request: &AcquireRequest<'_>,
    intent: &mut AcquisitionIntent,
    intent_file: &Path,
) -> Option<(Fingerprint, MaterializationPlan)> {
    let root = git::capture(request.cwd, &["rev-parse", "--show-toplevel"]).ok()?;
    let root = cache::canonical_path(Path::new(&root));
    let head = git::capture(&root, &["rev-parse", "--verify", "HEAD"]).ok()?;
    let status = git::capture(
        &root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .ok()?;
    if head != intent.base_oid || !status.is_empty() {
        return None;
    }
    planned(intent, intent_file, &root).ok()
}

fn record(
    plan: &MaterializationPlan,
    mode: MaterializationMode,
    cones: Vec<String>,
    source: Option<String>,
    candidate: Option<String>,
    reason: Option<FallbackReason>,
    started: Instant,
) -> Result<MaterializationRecord> {
    let full = mode == MaterializationMode::Full;
    let record = MaterializationRecord {
        schema_version: super::super::worktree_materialization::SCHEMA_VERSION,
        mode,
        requested_scopes: plan.requested_scopes.clone(),
        closure_cones: plan.closure_cones.clone(),
        support_cones: plan.support_cones.clone(),
        current_cones: cones,
        base_oid: plan.base_oid.clone(),
        source_cargo_fingerprint: source,
        candidate_cargo_fingerprint: candidate,
        full_tracked_files: plan.full_tracked_files,
        full_git_blob_bytes: plan.full_git_blob_bytes,
        full_working_files: plan.full_working_files,
        full_working_logical_bytes: plan.full_working_logical_bytes,
        selected_tracked_files: plan.selected_tracked_files,
        selected_git_blob_bytes: plan.selected_git_blob_bytes,
        working_files: if full {
            plan.full_working_files
        } else {
            plan.selected_working_files
        },
        working_logical_bytes: if full {
            plan.full_working_logical_bytes
        } else {
            plan.selected_working_logical_bytes
        },
        materialization_duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        materialized_at: now_secs(),
        expansion_count: 0,
        last_expanded_at: None,
        fallback_reason: reason,
    };
    record.validate()?;
    Ok(record)
}

fn full_record(
    workspace: &Path,
    state: &State,
    plan: &MaterializationPlan,
    source: &Fingerprint,
    reason: FallbackReason,
    candidate: Option<String>,
    started: Instant,
) -> Result<MaterializationRecord> {
    full(workspace).map_err(anyhow::Error::new)?;
    let proof = capture(&inside(workspace, &state.cargo_dir), workspace)?;
    if !equivalent(source, &proof)? {
        bail!("full fallback does not match the planned Cargo workspace")
    }
    record(
        plan,
        MaterializationMode::Full,
        Vec::new(),
        Some(fingerprint(source).to_string()),
        match reason {
            FallbackReason::MetadataFailed => None,
            FallbackReason::MetadataMismatch => candidate,
            _ => Some(fingerprint(&proof).to_string()),
        },
        Some(reason),
        started,
    )
}

pub(crate) fn scoped(
    request: &AcquireRequest<'_>,
    scopes: &[String],
    config: &config::Config,
) -> Result<PathBuf> {
    if scopes.is_empty() {
        return bind(request, config);
    }
    let ctx = repo_context(request.cwd)?;
    if let Some(lease) = recovered(request, &ctx)? {
        seed(request.root, &lease);
        return Ok(PathBuf::from(lease.workspace));
    }
    let root_dir = worktree_root(config, request.root, &ctx.repo_id, &ctx.main_root);
    fs::create_dir_all(&root_dir)?;
    let root_dir = cache::canonical_path(&root_dir);
    let default_branch = format!("grove/{}", sanitize(&request.agent));
    let initial = state(request, config, scopes)?;
    let lease = loop {
        let planned_slot = {
            let _git = repo_git_lock(request.root, &ctx.repo_id)?;
            slot(request, &ctx, &root_dir, &default_branch)?
        };
        let _lifecycle =
            cache::lifecycle_exclusive(request.root, &ctx.repo_id, &planned_slot.workspace)?;
        let _git = repo_git_lock(request.root, &ctx.repo_id)?;
        let current = slot(request, &ctx, &root_dir, &default_branch)?;
        if !same(&planned_slot, &current) {
            continue;
        }
        let mut intent = intent(request, &ctx, &current, Some(initial.clone()));
        let intent_file = write_intent(request.root, &intent)?;
        let preplanned = preplan(request, &mut intent, &intent_file);
        let planned_from_source = preplanned.is_some();
        add(&Add {
            main: &ctx.main_root,
            branch: &intent.branch,
            existing: current.existing,
            workspace: &current.workspace,
            base: &intent.base_oid,
            checkout: preplanned
                .as_ref()
                .is_none_or(|(_, plan)| plan.mode == MaterializationMode::Full),
        })
        .map_err(anyhow::Error::new)?;
        exact(&ctx, &intent)?;
        let started = Instant::now();
        let (source, plan) = match preplanned {
            Some(planned) => planned,
            None => planned(&mut intent, &intent_file, &current.workspace)?,
        };
        let state = intent.materialization.as_ref().unwrap();
        let materialization = if plan.mode == MaterializationMode::Full {
            let candidate = if planned_from_source {
                let candidate = capture(
                    &inside(&current.workspace, &state.cargo_dir),
                    &current.workspace,
                )?;
                if !equivalent(&source, &candidate)? {
                    bail!("full checkout does not match the planned Cargo workspace")
                }
                fingerprint(&candidate).to_string()
            } else {
                fingerprint(&source).to_string()
            };
            record(
                &plan,
                MaterializationMode::Full,
                Vec::new(),
                Some(fingerprint(&source).to_string()),
                Some(candidate),
                plan.fallback_reason,
                started,
            )?
        } else {
            let cones: Vec<_> = plan
                .closure_cones
                .iter()
                .chain(&plan.support_cones)
                .cloned()
                .collect();
            match sparse(&current.workspace, &cones) {
                Ok(actual) => {
                    let candidate = capture(
                        &inside(&current.workspace, &state.cargo_dir),
                        &current.workspace,
                    );
                    match candidate {
                        Ok(candidate) if equivalent(&source, &candidate)? => record(
                            &plan,
                            MaterializationMode::Sparse,
                            actual,
                            Some(fingerprint(&source).to_string()),
                            Some(fingerprint(&candidate).to_string()),
                            None,
                            started,
                        )?,
                        Ok(candidate) => full_record(
                            &current.workspace,
                            state,
                            &plan,
                            &source,
                            FallbackReason::MetadataMismatch,
                            Some(fingerprint(&candidate).to_string()),
                            started,
                        )?,
                        Err(_) => full_record(
                            &current.workspace,
                            state,
                            &plan,
                            &source,
                            FallbackReason::MetadataFailed,
                            None,
                            started,
                        )?,
                    }
                }
                Err(Failure::Unsupported(_)) => full_record(
                    &current.workspace,
                    state,
                    &plan,
                    &source,
                    FallbackReason::GitUnsupported,
                    None,
                    started,
                )?,
                Err(Failure::Setup(_)) => full_record(
                    &current.workspace,
                    state,
                    &plan,
                    &source,
                    FallbackReason::SparseSetupFailed,
                    None,
                    started,
                )?,
            }
        };
        exact(&ctx, &intent)?;
        break publish(request.root, &intent_file, &intent, Some(materialization))?;
    };
    seed(request.root, &lease);
    Ok(PathBuf::from(lease.workspace))
}

pub(super) fn recover(
    root: &Path,
    ctx: &RepoContext,
    intent_file: &Path,
    intent: &AcquisitionIntent,
) -> Result<Lease> {
    let workspace = Path::new(&intent.workspace);
    let mut state = intent
        .materialization
        .clone()
        .context("materialized recovery omitted its durable state")?;
    full(workspace).map_err(anyhow::Error::new)?;
    exact(ctx, intent)?;
    let cargo = inside(workspace, &state.cargo_dir);
    let source = capture(&cargo, workspace)?;
    let plan = match state.plan.take() {
        Some(plan) => plan,
        None => {
            let config = state.config.as_deref().map(|path| inside(workspace, path));
            plan(PlanInput {
                workspace: &cargo,
                base_oid: &intent.base_oid,
                scopes: &state.scopes,
                extras: &state.extras,
                config: config.as_deref(),
                fingerprint: &source,
                planned_at: now_secs(),
            })?
        }
    };
    if plan.cargo_fingerprint.as_deref() != Some(fingerprint(&source)) {
        bail!("recovered full checkout does not match its durable Cargo fingerprint")
    }
    let materialization = record(
        &plan,
        MaterializationMode::Full,
        Vec::new(),
        Some(fingerprint(&source).to_string()),
        Some(fingerprint(&source).to_string()),
        Some(FallbackReason::RecoveryFull),
        Instant::now(),
    )?;
    publish(root, intent_file, intent, Some(materialization))
}

#[cfg(test)]
#[path = "worktree_acquire_materialized_tests.rs"]
mod tests;
