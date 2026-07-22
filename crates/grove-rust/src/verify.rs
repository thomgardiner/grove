use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;

use crate::api::Grove;
use crate::{
    artifact, cache, claim, config, inspection_snapshot, project, snapshot, task, worktree,
};

#[path = "verify_receipt.rs"]
mod receipt;

#[path = "verify_portable.rs"]
mod portable;

#[path = "verify_retention.rs"]
mod retention;

pub use grove_core::verification::{Checkout, Evidence, LaneIdentity, Receipt};
pub use portable::PortableInputs;
use receipt::{checkout, receipts, runs};

#[path = "verify_dag.rs"]
mod dag;

#[path = "verify_query.rs"]
mod query;

pub use query::{PortableMatch, PortableQueryReport, PortableReceipt};

pub(crate) fn evidence_lock(root: &Path) -> Result<File> {
    grove_core::verification_retention::lock(root)
}

/// Digest of the workspace's complete verification policy (required profiles
/// plus every profile definition). Pinned at `task begin`; finish refuses on
/// drift unless the caller accepts the current digest with `--accept-policy`.
///
/// Bounds worth knowing: this binds the policy *document*, not everything a
/// verification command reads. A profile that shells out to `ci/verify.sh` can
/// be weakened by editing that script without moving this digest. Grove cannot
/// see those inputs; an orchestrator closes the gap by naming such files as
/// protected paths. Nor can Grove authenticate who passes `--accept-policy` —
/// like `--allow-unverified`, it makes drift a deliberate, recorded act rather
/// than an authorization boundary.
pub fn policy_sha256(config: &config::Config) -> String {
    dag::policy_sha256(config)
}

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

pub(crate) fn reclaim_evidence(root: &Path) -> Vec<String> {
    grove_core::verification_retention::reclaim(root, &retention::portable_runs(root))
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
    /// Present when finish compared the candidate against an inspection
    /// source digest under Grove's cooperative workspace and lifecycle locks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
}

/// One `task finish` attempt. Ordinary success keeps the exact `FinishReport`
/// JSON shape existing consumers parse; a source-bound success adds its digest.
/// Domain refusals use a machine-readable envelope on stdout (exit 1).
#[derive(Serialize)]
#[serde(untagged)]
pub enum FinishOutcome {
    Finished(Box<FinishReport>),
    Refused(Box<FinishRefusal>),
}

#[derive(Serialize)]
pub struct FinishRefusal {
    /// Always "refused"; the key distinguishes this envelope from a FinishReport.
    pub outcome: &'static str,
    /// "scope" (writes outside the declared scope), "evidence" (missing,
    /// stale, or failed required verification), "source_changed" (the
    /// candidate no longer matches the inspected source digest), or
    /// "policy_changed" (the verification policy no longer matches the digest
    /// pinned at task begin).
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outside_scope: Vec<String>,
    /// Present on "evidence": which required profiles are missing/stale/failed,
    /// so the caller can run exactly those and retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<TaskVerification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_source_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_source_sha256: Option<String>,
    /// Present on "policy_changed": the digest pinned at begin and the digest
    /// of the current policy. Re-run finish with `--accept-policy <actual>`
    /// after reviewing the policy diff to accept the change deliberately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_policy_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_policy_sha256: Option<String>,
}

fn source_changed(expected: &str, actual: String) -> FinishOutcome {
    FinishOutcome::Refused(Box::new(FinishRefusal {
        outcome: "refused",
        reason: "source_changed",
        outside_scope: Vec::new(),
        verification: None,
        expected_source_sha256: Some(expected.to_string()),
        actual_source_sha256: Some(actual),
        expected_policy_sha256: None,
        actual_policy_sha256: None,
    }))
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

pub fn finish(
    root: &Path,
    repo: &str,
    config: &config::Config,
    id: &str,
    allow_unverified: Option<&str>,
) -> Result<FinishOutcome> {
    finish_bound(root, repo, config, id, None, allow_unverified, None)
}

pub fn finish_bound(
    root: &Path,
    repo: &str,
    config: &config::Config,
    id: &str,
    expected_source_sha256: Option<&str>,
    allow_unverified: Option<&str>,
    accept_policy_sha256: Option<&str>,
) -> Result<FinishOutcome> {
    if let Some(expected) = expected_source_sha256
        && (expected.len() != 64
            || !expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
    {
        bail!("--expected-source-sha256 must be 64 lowercase hexadecimal characters");
    }
    let task = task::load(root, repo, id)?;
    let _workspace_lock = snapshot::workspace_lock(root, Path::new(&task.workspace))?;
    // Main's lock discipline (evidence lock outermost, registry lock scoped so
    // lease renewal happens after release) carrying this branch's machine-
    // readable refusals. Refusal paths return before renewal, exactly as the
    // pre-envelope bail paths did.
    let _evidence_lock = evidence_lock(root)?;
    let (task, verification) = {
        let _lock = claim::registry_lock(root, repo)?;
        if let Some(expected) = expected_source_sha256 {
            let actual = inspection_snapshot::digest(Path::new(&task.workspace))?;
            if actual != expected {
                return Ok(source_changed(expected, actual));
            }
        }
        let outside_scope = task::outside_scope(root, repo, id)?;
        if !outside_scope.is_empty() {
            return Ok(FinishOutcome::Refused(Box::new(FinishRefusal {
                outcome: "refused",
                reason: "scope",
                outside_scope,
                verification: None,
                expected_source_sha256: None,
                actual_source_sha256: None,
                expected_policy_sha256: None,
                actual_policy_sha256: None,
            })));
        }
        // The policy the candidate is judged by must be the policy pinned when
        // the task began; otherwise the candidate silently controls its own
        // acceptance bar. This `config` is the same snapshot the receipts are
        // evaluated against below, so the digest and the evaluation cannot
        // disagree about which policy applied.
        if let Some(pinned) = task.verification_policy_sha256.as_deref() {
            let current = policy_sha256(config);
            if current != pinned && accept_policy_sha256 != Some(current.as_str()) {
                return Ok(FinishOutcome::Refused(Box::new(FinishRefusal {
                    outcome: "refused",
                    reason: "policy_changed",
                    outside_scope: Vec::new(),
                    verification: None,
                    expected_source_sha256: None,
                    actual_source_sha256: None,
                    expected_policy_sha256: Some(pinned.to_string()),
                    actual_policy_sha256: Some(current),
                })));
            }
        }
        let verification = task_verification_locked(root, repo, id, Some(config))?;
        let (state, reason) = if verification.verified {
            (task::Verification::Passed, None)
        } else if let Some(reason) = allow_unverified.filter(|reason| !reason.trim().is_empty()) {
            (task::Verification::Overridden, Some(reason.to_string()))
        } else {
            return Ok(FinishOutcome::Refused(Box::new(FinishRefusal {
                outcome: "refused",
                reason: "evidence",
                outside_scope: Vec::new(),
                verification: Some(verification),
                expected_source_sha256: None,
                actual_source_sha256: None,
                expected_policy_sha256: None,
                actual_policy_sha256: None,
            })));
        };
        if let Some(expected) = expected_source_sha256 {
            let actual = inspection_snapshot::digest(Path::new(&task.workspace))?;
            if actual != expected {
                return Ok(source_changed(expected, actual));
            }
        }
        let task = task::finish_with_verification_locked(
            root,
            repo,
            id,
            state,
            reason,
            expected_source_sha256.map(str::to_string),
        )?;
        (task, verification)
    };
    task::renew(root, &task);
    let source_sha256 = task.source_sha256.clone();
    Ok(FinishOutcome::Finished(Box::new(FinishReport {
        task,
        verification,
        source_sha256,
    })))
}
