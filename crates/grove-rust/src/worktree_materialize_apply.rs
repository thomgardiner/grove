use super::*;
use std::time::Instant;

use crate::worktree::worktree_materialization::{actual_cones, full as checkout_full, sparse};

pub(super) fn reconcile(
    root: &Path,
    locked: &Locked,
    intent: MaterializationIntent,
) -> Result<MaterializationRecord> {
    intent.validate()?;
    identity(locked, &intent)?;
    if locked.lease.materialization.as_ref() != Some(&intent.prior) {
        let current = locked
            .lease
            .materialization
            .clone()
            .context("materialization intent conflicts with a legacy lease")?;
        if completed(&intent, &current) {
            fs::remove_file(&locked.intent)?;
            return Ok(current);
        }
        bail!("materialization intent conflicts with the durable lease")
    }
    let started = Instant::now();
    if intent.desired_mode == MaterializationMode::Sparse
        && actual_cones(&locked.workspace)
            .map_err(anyhow::Error::new)?
            .is_none()
    {
        return publish(
            root,
            locked,
            &intent,
            Vec::new(),
            Some(FallbackReason::RecoveryFull),
            started,
        );
    }
    let actual = apply(locked, &intent)?;
    if intent.desired_mode == MaterializationMode::Full
        && actual_cones(&locked.workspace)
            .map_err(anyhow::Error::new)?
            .is_some()
    {
        bail!("full conversion left sparse checkout enabled")
    }
    publish(root, locked, &intent, actual, None, started)
}

fn apply(locked: &Locked, intent: &MaterializationIntent) -> Result<Vec<String>> {
    match intent.desired_mode {
        MaterializationMode::Full => {
            checkout_full(&locked.workspace).map_err(anyhow::Error::new)?;
            Ok(Vec::new())
        }
        MaterializationMode::Sparse => {
            let actual = actual_cones(&locked.workspace)
                .map_err(anyhow::Error::new)?
                .context("sparse checkout disappeared during expansion")?;
            let desired = union(&intent.desired_cones, actual.iter().cloned());
            sparse(&locked.workspace, &desired).map_err(anyhow::Error::new)
        }
    }
}

fn publish(
    root: &Path,
    locked: &Locked,
    intent: &MaterializationIntent,
    actual: Vec<String>,
    reason: Option<FallbackReason>,
    started: Instant,
) -> Result<MaterializationRecord> {
    if intent.desired_mode == MaterializationMode::Sparse
        && !covers_all(&actual, &intent.desired_cones)
    {
        bail!("sparse expansion did not retain every prior and requested cone")
    }
    let cargo = inside(&locked.workspace, &intent.cargo_dir);
    let candidate = capture(&cargo, &locked.workspace);
    let mut durable = durable(locked, intent, candidate.as_ref().ok())?;
    if durable.desired_mode == MaterializationMode::Full {
        let (files, bytes) = measure(&locked.workspace, &durable.base_oid)?;
        durable.prior.full_working_files = files;
        durable.prior.full_working_logical_bytes = bytes;
        if let Some(plan) = durable.plan.as_mut() {
            plan.full_working_files = files;
            plan.full_working_logical_bytes = bytes;
            plan.selected_working_files = files;
            plan.selected_working_logical_bytes = bytes;
        }
    }
    let record = record(&durable, actual, candidate.as_ref().ok(), reason, started)?;
    let mut lease = locked.lease.clone();
    lease.materialization = Some(record.clone());
    lease.last_activity = now_secs();
    write_lease(root, &lease)?;
    fs::remove_file(&locked.intent)?;
    Ok(record)
}

fn durable(
    locked: &Locked,
    intent: &MaterializationIntent,
    candidate: Option<&Fingerprint>,
) -> Result<MaterializationIntent> {
    if intent.desired_mode == MaterializationMode::Full {
        return Ok(intent.clone());
    }
    let source = candidate.context("expanded Cargo metadata could not be captured")?;
    let config = config::Config::resolve(&locked.cargo);
    let mut plan = selection(locked, &config, &intent.requested_scopes, source)?;
    full_metrics(&mut plan, &intent.prior)?;
    if plan.mode != MaterializationMode::Sparse
        || plan.requested_scopes != intent.requested_scopes
        || plan.closure_cones != intent.closure_cones
        || plan.support_cones != intent.support_cones
    {
        bail!("expanded materialization plan changed after Git mutation")
    }
    let mut durable = intent.clone();
    durable.plan = Some(plan);
    durable.validate()?;
    Ok(durable)
}

fn record(
    intent: &MaterializationIntent,
    actual: Vec<String>,
    candidate: Option<&Fingerprint>,
    forced: Option<FallbackReason>,
    started: Instant,
) -> Result<MaterializationRecord> {
    let full = intent.desired_mode == MaterializationMode::Full;
    let plan = intent.plan.as_ref();
    let source = intent.prior.source_cargo_fingerprint.clone();
    let candidate = candidate.map(|value| fingerprint(value).to_string());
    let reason = forced.or_else(|| {
        if candidate.is_none() {
            Some(FallbackReason::MetadataFailed)
        } else if candidate != source {
            Some(FallbackReason::MetadataMismatch)
        } else {
            plan.and_then(|plan| plan.fallback_reason)
        }
    });
    let record = MaterializationRecord {
        schema_version: SCHEMA_VERSION,
        mode: intent.desired_mode,
        requested_scopes: intent.requested_scopes.clone(),
        closure_cones: intent.closure_cones.clone(),
        support_cones: intent.support_cones.clone(),
        current_cones: actual,
        base_oid: intent.base_oid.clone(),
        source_cargo_fingerprint: source,
        candidate_cargo_fingerprint: candidate,
        full_tracked_files: plan.map_or(intent.prior.full_tracked_files, |p| p.full_tracked_files),
        full_git_blob_bytes: plan
            .map_or(intent.prior.full_git_blob_bytes, |p| p.full_git_blob_bytes),
        full_working_files: plan.map_or(intent.prior.full_working_files, |p| p.full_working_files),
        full_working_logical_bytes: plan.map_or(intent.prior.full_working_logical_bytes, |p| {
            p.full_working_logical_bytes
        }),
        selected_tracked_files: selected_files(intent, full)?,
        selected_git_blob_bytes: selected_bytes(intent, full)?,
        working_files: working_files(intent, full)?,
        working_logical_bytes: working_bytes(intent, full)?,
        materialization_duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        materialized_at: now_secs(),
        expansion_count: intent.prior.expansion_count.saturating_add(1),
        last_expanded_at: Some(now_secs()),
        fallback_reason: reason,
    };
    record.validate()?;
    Ok(record)
}

fn selected_files(intent: &MaterializationIntent, full: bool) -> Result<u64> {
    Ok(if full {
        intent
            .plan
            .as_ref()
            .map_or(intent.prior.full_tracked_files, |p| p.full_tracked_files)
    } else {
        plan(intent)?.selected_tracked_files
    })
}

fn selected_bytes(intent: &MaterializationIntent, full: bool) -> Result<u64> {
    Ok(if full {
        intent
            .plan
            .as_ref()
            .map_or(intent.prior.full_git_blob_bytes, |p| p.full_git_blob_bytes)
    } else {
        plan(intent)?.selected_git_blob_bytes
    })
}

fn working_files(intent: &MaterializationIntent, full: bool) -> Result<u64> {
    Ok(if full {
        intent
            .plan
            .as_ref()
            .map_or(intent.prior.full_working_files, |p| p.full_working_files)
    } else {
        plan(intent)?.selected_working_files
    })
}

fn working_bytes(intent: &MaterializationIntent, full: bool) -> Result<u64> {
    Ok(if full {
        intent
            .plan
            .as_ref()
            .map_or(intent.prior.full_working_logical_bytes, |p| {
                p.full_working_logical_bytes
            })
    } else {
        plan(intent)?.selected_working_logical_bytes
    })
}

fn plan(intent: &MaterializationIntent) -> Result<&MaterializationPlan> {
    intent
        .plan
        .as_ref()
        .context("sparse expansion omitted its plan")
}

fn identity(locked: &Locked, intent: &MaterializationIntent) -> Result<()> {
    if intent.repo != locked.lease.repo
        || intent.workspace != locked.lease.workspace
        || intent.branch != locked.lease.branch
        || intent.base_oid != locked.lease.base_oid
        || !is_our_leased_worktree(&locked.lease)
    {
        bail!("materialization intent does not match its managed worktree")
    }
    Ok(())
}

fn completed(intent: &MaterializationIntent, record: &MaterializationRecord) -> bool {
    record.validate().is_ok()
        && record.base_oid == intent.base_oid
        && match intent.desired_mode {
            MaterializationMode::Full => record.mode == MaterializationMode::Full,
            MaterializationMode::Sparse => {
                record.mode == MaterializationMode::Sparse
                    && covers_all(&record.current_cones, &intent.desired_cones)
            }
        }
}

fn covers_all(cones: &[String], required: &[String]) -> bool {
    required.iter().all(|required| {
        cones.iter().any(|cone| {
            required == cone
                || required
                    .strip_prefix(cone)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    })
}

fn inside(workspace: &Path, relative: &str) -> PathBuf {
    if relative == "." {
        workspace.to_path_buf()
    } else {
        workspace.join(relative)
    }
}
