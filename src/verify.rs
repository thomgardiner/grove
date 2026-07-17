//! Repository-declared verification profiles and durable command receipts.
//!
//! A receipt is evidence of one command at one checkout state. It deliberately does
//! not claim that the command proves behavior, performance, security, or visual output.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;

use crate::api::Grove;
use crate::{artifact, cache, claim, config, project, snapshot, task, worktree};

#[path = "verify_receipt.rs"]
mod receipt;

#[path = "verify_portable.rs"]
mod portable;

#[path = "verify_retention.rs"]
mod retention;

pub use portable::PortableInputs;
pub use receipt::{Checkout, Evidence, LaneIdentity, Receipt};
use receipt::{checkout, receipts, runs};

#[path = "verify_dag.rs"]
mod dag;

#[path = "verify_query.rs"]
mod query;

pub use query::{PortableMatch, PortableQueryReport, PortableReceipt};

/// Hold evidence publication or reads against cache GC. Independent holders share this
/// lock; GC takes the same file exclusively and non-blockingly.
pub(crate) fn evidence_lock(root: &Path) -> Result<File> {
    retention::lock(root)
}

/// Run a frozen profile while the caller holds [`evidence_lock`]. This lets frozen
/// release cover its initial snapshot through bundle publication with one lock.
#[cfg(unix)]
pub(crate) fn run_locked_in_lane_with_lock(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    name: &str,
    task_id: Option<&str>,
    lane_tag: &str,
    lane: &cache::Lane,
) -> Result<VerifyReport> {
    dag::run_locked_in_lane(root, workspace, config, name, task_id, lane_tag, lane)
}

/// Reclaim superseded receipt graphs while retaining the latest run each task verifier
/// can select. Cache GC owns scheduling; verification owns the on-disk evidence format.
pub(crate) fn reclaim_evidence(root: &Path) -> Vec<String> {
    retention::reclaim(root)
}

#[derive(Serialize)]
pub struct VerifyReport {
    pub profile: String,
    pub run_id: String,
    pub passed: bool,
    pub receipts: Vec<Receipt>,
}

#[derive(Serialize)]
pub struct TaskVerification {
    pub required: Vec<String>,
    pub passed: Vec<String>,
    pub missing: Vec<String>,
    pub stale: Vec<String>,
    pub failed: Vec<String>,
    pub verified: bool,
}

#[derive(Serialize)]
pub struct FinishReport {
    pub task: task::Task,
    pub verification: TaskVerification,
}

fn profile(config: &config::Config, name: &str) -> Result<(config::VerificationProfile, bool)> {
    select(config, name)
}

fn select(config: &config::Config, name: &str) -> Result<(config::VerificationProfile, bool)> {
    let verification = config
        .verification
        .as_ref()
        .context("no verification profiles are configured in .grove.toml")?;
    let profile = verification
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("no verification profile named {name:?}"))?;
    if profile.commands.is_empty() {
        bail!("verification profile {name:?} has no commands");
    }
    if profile.continue_on_failure.is_none() {
        bail!("verification profile {name:?} must declare continue_on_failure");
    }
    portable::validate_env(&profile.portable_env)?;
    for (index, command) in profile.commands.iter().enumerate() {
        if command.argv.is_empty() {
            bail!(
                "verification profile {name:?} command {} has no argv",
                index + 1
            );
        }
        if command.allow_zero_tests.is_none() {
            bail!(
                "verification profile {name:?} command {} must declare allow_zero_tests",
                index + 1
            );
        }
    }
    dag::validate(&profile)?;
    Ok((
        profile,
        verification
            .required
            .iter()
            .any(|required| required == name),
    ))
}

/// Run one named profile in dedicated seeded lane(s). Every started command produces a
/// receipt, and default profiles preserve the established one-lane serial execution.
pub fn run(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    name: &str,
    task_id: Option<&str>,
) -> Result<VerifyReport> {
    let workspace = cache::canonical_path(workspace);
    worktree::full(root, &workspace)?;
    let _workspace_lock = snapshot::workspace_lock(root, &workspace)?;
    run_locked(root, &workspace, config, name, task_id)
}

/// Run while the caller holds this workspace's snapshot lock. Frozen release uses this
/// to make snapshot → verification → export one cooperative transaction.
pub(crate) fn run_locked(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    name: &str,
    task_id: Option<&str>,
) -> Result<VerifyReport> {
    let _evidence_lock = evidence_lock(root)?;
    let report = dag::run(root, workspace, config, name, task_id)?;
    crate::events::record(
        root,
        &crate::project::repo_identity(workspace),
        "verify.completed",
        serde_json::json!({"profile": name, "passed": report.passed, "task_id": task_id}),
    );
    Ok(report)
}

pub fn query(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    name: &str,
) -> Result<PortableQueryReport> {
    query::run(root, workspace, config, name)
}

/// Compare durable receipts with the task's *current* checkout. A profile run on an
/// older diff is deliberately not reused as verification for later changed work.
pub(crate) fn task_verification(root: &Path, repo: &str, id: &str) -> Result<TaskVerification> {
    let _evidence_lock = evidence_lock(root)?;
    task_verification_locked(root, repo, id, None)
}

fn task_verification_locked(
    root: &Path,
    repo: &str,
    id: &str,
    config: Option<&config::Config>,
) -> Result<TaskVerification> {
    let task = task::load(root, repo, id)?;
    let workspace = Path::new(&task.workspace);
    let owned = config.is_none().then(|| config::Config::resolve(workspace));
    let config = config.or(owned.as_ref()).unwrap();
    let required = config
        .verification
        .as_ref()
        .map(|verification| verification.required.clone())
        .unwrap_or_default();
    if required.is_empty() {
        return Ok(TaskVerification {
            required,
            passed: Vec::new(),
            missing: Vec::new(),
            stale: Vec::new(),
            failed: Vec::new(),
            verified: false,
        });
    }
    let expected = snapshot::capture(workspace)?.reference();
    let expected_checkout = checkout(workspace);
    let receipts = receipts(root, repo)?;
    let runs = runs(root, repo)?;
    let mut passed = Vec::new();
    let mut missing = Vec::new();
    let mut stale = Vec::new();
    let mut failed = Vec::new();
    for required_profile in &required {
        let (configured, _) = select(config, required_profile)?;
        let command_count = configured.commands.len();
        let expected_profile_sha256 = dag::profile_sha256(&configured);
        let mut receipt_runs = BTreeMap::<String, Vec<&Receipt>>::new();
        for receipt in receipts.iter().filter(|receipt| {
            receipt.task_id.as_deref() == Some(id) && receipt.profile == *required_profile
        }) {
            receipt_runs
                .entry(receipt.run_id.clone())
                .or_default()
                .push(receipt);
        }
        let latest = runs
            .iter()
            .filter(|run| {
                run.task_id.as_deref() == Some(id)
                    && run.profile == *required_profile
                    && run.schema_version == 1
            })
            .max_by_key(|run| (run.completed_at_nanos, &run.run_id));
        let Some(record) = latest else {
            missing.push(required_profile.clone());
            continue;
        };
        let Some(run) = receipt_runs.remove(&record.run_id) else {
            failed.push(required_profile.clone());
            continue;
        };
        let current = run.iter().all(|receipt| {
            receipt.evidence.as_ref().is_some_and(|evidence| {
                evidence.input == expected
                    && evidence.output == expected
                    && evidence.checkout == expected_checkout
                    && snapshot::validate(root, repo, &evidence.input).is_ok()
                    && snapshot::validate(root, repo, &evidence.output).is_ok()
            })
        });
        let complete = record.command_count == command_count
            && record.receipt_count == run.len()
            && record.profile_sha256 == expected_profile_sha256
            && record.passed == run.iter().all(|receipt| receipt.passed)
            && run.len() == command_count
            && run.iter().all(|receipt| {
                configured
                    .commands
                    .get(receipt.command_index)
                    .is_some_and(|command| command.argv == receipt.argv)
                    && receipt.profile_sha256 == expected_profile_sha256
            })
            && run
                .iter()
                .map(|receipt| receipt.command_index)
                .collect::<BTreeSet<_>>()
                .len()
                == command_count;
        if !current {
            stale.push(required_profile.clone());
        } else if complete && run.iter().all(|receipt| receipt.passed) {
            passed.push(required_profile.clone());
        } else {
            failed.push(required_profile.clone());
        }
    }
    Ok(TaskVerification {
        verified: missing.is_empty() && stale.is_empty() && failed.is_empty(),
        required,
        passed,
        missing,
        stale,
        failed,
    })
}

/// Assess the required receipts for a task in `workspace`. This is also used by artifact
/// export so it can report `verified: true` only when an explicit task proves its
/// required profile commands were run for the current checkout.
pub fn task_verification_for_workspace(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    id: &str,
) -> Result<TaskVerification> {
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    let task = task::load(root, &repo, id)?;
    if task.workspace != workspace.to_string_lossy() {
        bail!("task {id} belongs to a different workspace");
    }
    let _evidence_lock = evidence_lock(root)?;
    task_verification_locked(root, &repo, id, Some(config))
}

/// Gate an artifact export on a task bound to this exact workspace. A caller without
/// fresh evidence must supply an explicit reason, which artifact export persists.
pub fn export(
    grove: &Grove,
    tag: &str,
    source: &Path,
    destination: &Path,
    task_id: Option<&str>,
    allow_unverified: Option<String>,
) -> Result<artifact::Export> {
    let root = grove.root();
    let workspace = grove.workspace();
    let _workspace_lock = snapshot::workspace_lock(root, workspace)?;
    match task_id {
        Some(id)
            if task_verification_for_workspace(root, workspace, grove.config(), id)?.verified =>
        {
            artifact::export_verified(grove, tag, source, destination)
        }
        _ => artifact::export_override(
            grove,
            tag,
            source,
            destination,
            allow_unverified
                .filter(|reason| !reason.trim().is_empty())
                .context(
                    "artifact export requires fresh task verification or --allow-unverified REASON",
                )?,
        ),
    }
}

/// Finish only with fresh required evidence, unless the caller records an explicit override.
pub fn finish(
    root: &Path,
    repo: &str,
    config: &config::Config,
    id: &str,
    allow_unverified: Option<&str>,
) -> Result<FinishReport> {
    let task = task::load(root, repo, id)?;
    let _workspace_lock = snapshot::workspace_lock(root, Path::new(&task.workspace))?;
    let _evidence_lock = evidence_lock(root)?;
    let (task, verification) = {
        let _lock = claim::registry_lock(root, repo)?;
        let outside_scope = task::outside_scope(root, repo, id)?;
        if !outside_scope.is_empty() {
            bail!(
                "task {id} wrote outside its declared scope: {}",
                outside_scope.join(", ")
            );
        }
        let verification = task_verification_locked(root, repo, id, Some(config))?;
        let (state, reason) = if verification.verified {
            (task::Verification::Passed, None)
        } else if let Some(reason) = allow_unverified.filter(|reason| !reason.trim().is_empty()) {
            (task::Verification::Overridden, Some(reason.to_string()))
        } else {
            bail!(
                "task {id} lacks fresh required verification (missing: {}; stale: {}; failed: {}); rerun verification or pass --allow-unverified REASON",
                verification.missing.join(", "),
                verification.stale.join(", "),
                verification.failed.join(", ")
            );
        };
        let task = task::finish_with_verification_locked(root, repo, id, state, reason)?;
        (task, verification)
    };
    task::renew(root, &task);
    Ok(FinishReport { task, verification })
}
