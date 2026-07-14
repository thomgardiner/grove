//! Repository-declared verification profiles and durable command receipts.
//!
//! A receipt is evidence of one command at one checkout state. It deliberately does
//! not claim that the command proves behavior, performance, security, or visual output.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;

use crate::api::Grove;
use crate::{cache, config, project, task};

#[path = "verify_receipt.rs"]
mod receipt;

pub use receipt::{Checkout, LaneIdentity, Receipt};
use receipt::{ReceiptContext, checkout, execute, receipts};

#[derive(Serialize)]
pub struct VerifyReport {
    pub profile: String,
    pub passed: bool,
    pub receipts: Vec<Receipt>,
}

#[derive(Serialize)]
pub struct TaskVerification {
    pub required: Vec<String>,
    pub passed: Vec<String>,
    pub missing: Vec<String>,
    pub failed: Vec<String>,
    pub verified: bool,
}

#[derive(Serialize)]
pub struct FinishReport {
    pub task: task::Task,
    pub verification: TaskVerification,
}

fn profile(name: &str) -> Result<(config::VerificationProfile, bool)> {
    let verification = config::get()
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
    Ok((
        profile,
        verification
            .required
            .iter()
            .any(|required| required == name),
    ))
}

/// Run one named profile in a dedicated seeded lane. Every command produces a receipt,
/// even a command that could not be spawned.
pub fn run(
    root: &Path,
    workspace: &Path,
    name: &str,
    task_id: Option<&str>,
) -> Result<VerifyReport> {
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    let snapshot = match task_id {
        Some(id) => {
            let task = task::load(root, &repo, id)?;
            if task.workspace != workspace.to_string_lossy() {
                bail!("task {id} belongs to a different workspace");
            }
            Some(task)
        }
        None => None,
    };
    let (profile, required) = profile(name)?;
    let lane = Grove::with_root(root.to_path_buf(), &workspace)
        .seeded_tagged_lane(&format!("verify-{name}"))?;
    let continue_on_failure = profile.continue_on_failure.unwrap_or(false);
    let command_count = profile.commands.len();
    let mut receipts = Vec::new();
    let context = ReceiptContext {
        root,
        workspace: &workspace,
        repo: &repo,
        task: snapshot.as_ref(),
        profile: name,
        required,
        lane: &lane,
    };
    for command in profile.commands {
        let receipt = execute(
            &context,
            &command.argv,
            command.allow_zero_tests.unwrap_or(false),
        )?;
        let passed = receipt.passed;
        receipts.push(receipt);
        if !passed && !continue_on_failure {
            break;
        }
    }
    Ok(VerifyReport {
        profile: name.to_string(),
        passed: receipts.len() == command_count && receipts.iter().all(|receipt| receipt.passed),
        receipts,
    })
}

/// Compare durable receipts with the task's *current* checkout. A profile run on an
/// older diff is deliberately not reused as verification for later changed work.
pub(crate) fn task_verification(root: &Path, repo: &str, id: &str) -> Result<TaskVerification> {
    let required = config::get()
        .verification
        .as_ref()
        .map(|verification| verification.required.clone())
        .unwrap_or_default();
    if required.is_empty() {
        return Ok(TaskVerification {
            required,
            passed: Vec::new(),
            missing: Vec::new(),
            failed: Vec::new(),
            verified: false,
        });
    }
    let task = task::load(root, repo, id)?;
    let expected = checkout(Path::new(&task.workspace));
    let receipts = receipts(root, repo)?;
    let mut passed = Vec::new();
    let mut missing = Vec::new();
    let mut failed = Vec::new();
    for profile in &required {
        let latest = receipts
            .iter()
            .filter(|receipt| {
                receipt.task_id.as_deref() == Some(id)
                    && receipt.profile == *profile
                    && receipt.checkout == expected
            })
            .max_by_key(|receipt| receipt.ended_at);
        match latest {
            Some(receipt) if receipt.passed => passed.push(profile.clone()),
            Some(_) => failed.push(profile.clone()),
            None => missing.push(profile.clone()),
        }
    }
    Ok(TaskVerification {
        verified: missing.is_empty() && failed.is_empty(),
        required,
        passed,
        missing,
        failed,
    })
}

/// Assess the required receipts for a task in `workspace`. This is also used by artifact
/// export so it can report `verified: true` only when an explicit task proves its
/// required profile commands were run for the current checkout.
pub fn task_verification_for_workspace(
    root: &Path,
    workspace: &Path,
    id: &str,
) -> Result<TaskVerification> {
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    task_verification(root, &repo, id)
}

/// Finish a task while recording whether the repository's required profiles supplied
/// evidence for this exact checkout. Missing evidence finishes the task unverified;
/// it never gets silently upgraded to a verified handoff.
pub fn finish(root: &Path, repo: &str, id: &str) -> Result<FinishReport> {
    let verification = task_verification(root, repo, id)?;
    let state = if verification.verified {
        task::Verification::Passed
    } else if verification.failed.is_empty() {
        task::Verification::Unverified
    } else {
        task::Verification::Failed
    };
    let task = task::finish_with_verification(root, repo, id, state)?;
    Ok(FinishReport { task, verification })
}
