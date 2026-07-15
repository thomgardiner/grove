//! Resource-bounded scheduling for repository-declared verification commands.

use anyhow::{Context, Result, bail};
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use super::receipt::{ReceiptContext, complete_run, execute, now_nanos};
use super::{Receipt, VerifyReport, profile};
use crate::api::Grove;
use crate::{cache, config, project, task};

#[path = "verify_dag_plan.rs"]
mod plan;
use plan::Plan;

pub(super) fn validate(profile: &config::VerificationProfile) -> Result<()> {
    plan::validate(profile)
}

pub(super) fn profile_sha256(profile: &config::VerificationProfile) -> String {
    plan::profile_sha256(profile)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Running,
    Passed,
    Failed,
    Blocked,
}

pub(super) fn run(
    root: &std::path::Path,
    workspace: &std::path::Path,
    name: &str,
    task_id: Option<&str>,
) -> Result<VerifyReport> {
    let (profile, required) = profile(name)?;
    let plan = Plan::new(&profile)?;
    cache::maintain(root, || {
        if plan.legacy_serial() {
            let lane = Grove::with_root(root.to_path_buf(), workspace)
                .seeded_tagged_lane(&format!("verify-{name}"))?;
            serial(root, workspace, name, task_id, &profile, required, &lane)
        } else {
            parallel(root, workspace, name, task_id, &profile, required, &plan)
        }
    })
}

/// Frozen release holds the lane through staging, so it only accepts the established
/// single-lane profile shape until multi-lane artifact provenance is modeled.
pub(crate) fn run_locked_in_lane(
    root: &std::path::Path,
    workspace: &std::path::Path,
    name: &str,
    task_id: Option<&str>,
    lane: &cache::Lane,
) -> Result<VerifyReport> {
    let (profile, required) = profile(name)?;
    let plan = Plan::new(&profile)?;
    if !plan.legacy_serial() {
        bail!("frozen release requires a serial verification profile")
    }
    serial(root, workspace, name, task_id, &profile, required, lane)
}

fn task_context(
    root: &std::path::Path,
    workspace: &std::path::Path,
    task_id: Option<&str>,
) -> Result<(String, Option<task::Task>)> {
    let repo = project::repo_identity(workspace);
    let task = match task_id {
        Some(id) => {
            let task = task::load(root, &repo, id)?;
            if task.workspace != workspace.to_string_lossy() {
                bail!("task {id} belongs to a different workspace")
            }
            Some(task)
        }
        None => None,
    };
    Ok((repo, task))
}

fn serial(
    root: &std::path::Path,
    workspace: &std::path::Path,
    name: &str,
    task_id: Option<&str>,
    profile: &config::VerificationProfile,
    required: bool,
    lane: &cache::Lane,
) -> Result<VerifyReport> {
    let (repo, task) = task_context(root, workspace, task_id)?;
    let run_id = run_id();
    let profile_sha256 = profile_sha256(profile);
    let lane_tag = format!("verify-{name}");
    let mut receipts = Vec::new();
    for (command_index, command) in profile.commands.iter().enumerate() {
        let context = ReceiptContext {
            root,
            workspace,
            repo: &repo,
            task: task.as_ref(),
            profile: name,
            run_id: &run_id,
            profile_sha256: &profile_sha256,
            command_index,
            required,
            lane_tag: &lane_tag,
            lane,
        };
        let receipt = execute(
            &context,
            &command.argv,
            command.allow_zero_tests.unwrap_or(false),
        )?;
        let passed = receipt.passed;
        receipts.push(receipt);
        if !passed && !profile.continue_on_failure.unwrap_or(false) {
            break;
        }
    }
    finish(
        root,
        &repo,
        task.as_ref(),
        name,
        run_id,
        profile_sha256,
        profile.commands.len(),
        receipts,
    )
}

fn parallel(
    root: &std::path::Path,
    workspace: &std::path::Path,
    name: &str,
    task_id: Option<&str>,
    profile: &config::VerificationProfile,
    required: bool,
    plan: &Plan,
) -> Result<VerifyReport> {
    let (repo, task) = task_context(root, workspace, task_id)?;
    let run_id = run_id();
    let profile_sha256 = profile_sha256(profile);
    let count = profile.commands.len();
    let mut states = vec![State::Pending; count];
    let mut receipts = (0..count).map(|_| None).collect::<Vec<Option<Receipt>>>();
    let mut used_cpu = 0;
    let mut used_memory = 0;
    let mut running = 0;
    let mut error = None;
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| -> Result<()> {
        loop {
            block_dependents(
                &mut states,
                plan,
                profile.continue_on_failure.unwrap_or(false),
            );
            while let Some(index) = launchable(
                &states,
                plan,
                used_cpu,
                used_memory,
                profile.continue_on_failure.unwrap_or(false),
            ) {
                let command = &profile.commands[index];
                let node = &plan.nodes[index];
                states[index] = State::Running;
                used_cpu += node.cpu;
                used_memory += node.memory_mib;
                running += 1;
                let sender = sender.clone();
                let repo = repo.clone();
                let task = task.clone();
                let argv = command.argv.clone();
                let allow_zero_tests = command.allow_zero_tests.unwrap_or(false);
                let lane_tag = format!("verify-{name}-{}", node.id);
                let profile_sha256 = profile_sha256.clone();
                let run_id = run_id.clone();
                let cpu = node.cpu;
                let memory_mib = node.memory_mib;
                scope.spawn(move || {
                    let result = worker(
                        root,
                        workspace,
                        &repo,
                        task.as_ref(),
                        name,
                        &run_id,
                        &profile_sha256,
                        index,
                        required,
                        &lane_tag,
                        &argv,
                        allow_zero_tests,
                    );
                    let _ = sender.send((index, cpu, memory_mib, result));
                });
            }
            if running == 0 {
                if states.iter().all(|state| *state != State::Pending) {
                    break;
                }
                bail!("verification scheduler could not launch a ready command")
            }
            let (index, cpu, memory_mib, result) = receiver
                .recv()
                .context("verification worker stopped without a receipt")?;
            running -= 1;
            used_cpu -= cpu;
            used_memory -= memory_mib;
            match result {
                Ok(receipt) => {
                    states[index] = if receipt.passed {
                        State::Passed
                    } else {
                        State::Failed
                    };
                    receipts[index] = Some(receipt);
                }
                Err(cause) => {
                    states[index] = State::Failed;
                    if error.is_none() {
                        error = Some(cause);
                    }
                }
            }
        }
        Ok(())
    })?;
    if let Some(error) = error {
        return Err(error);
    }
    finish(
        root,
        &repo,
        task.as_ref(),
        name,
        run_id,
        profile_sha256,
        count,
        receipts.into_iter().flatten().collect(),
    )
}

#[allow(clippy::too_many_arguments)]
fn worker(
    root: &std::path::Path,
    workspace: &std::path::Path,
    repo: &str,
    task: Option<&task::Task>,
    profile: &str,
    run_id: &str,
    profile_sha256: &str,
    command_index: usize,
    required: bool,
    lane_tag: &str,
    argv: &[String],
    allow_zero_tests: bool,
) -> Result<Receipt> {
    let lane = Grove::with_root(root.to_path_buf(), workspace).seeded_tagged_lane(lane_tag)?;
    let context = ReceiptContext {
        root,
        workspace,
        repo,
        task,
        profile,
        run_id,
        profile_sha256,
        command_index,
        required,
        lane_tag,
        lane: &lane,
    };
    execute(&context, argv, allow_zero_tests)
}

#[allow(clippy::too_many_arguments)]
fn finish(
    root: &std::path::Path,
    repo: &str,
    task: Option<&task::Task>,
    name: &str,
    run_id: String,
    profile_sha256: String,
    command_count: usize,
    receipts: Vec<Receipt>,
) -> Result<VerifyReport> {
    let run = super::receipt::Run {
        schema_version: 1,
        repository: repo.into(),
        task_id: task.map(|task| task.id.clone()),
        profile: name.into(),
        run_id: run_id.clone(),
        profile_sha256: profile_sha256.clone(),
        command_count,
        receipt_count: receipts.len(),
        passed: receipts.len() == command_count && receipts.iter().all(|receipt| receipt.passed),
        completed_at_nanos: now_nanos(),
    };
    complete_run(root, repo, &run)?;
    Ok(VerifyReport {
        profile: name.to_string(),
        run_id,
        passed: receipts.len() == command_count && receipts.iter().all(|receipt| receipt.passed),
        receipts,
    })
}

fn block_dependents(states: &mut [State], plan: &Plan, continue_on_failure: bool) {
    let stopped = !continue_on_failure && states.contains(&State::Failed);
    for (index, node) in plan.nodes.iter().enumerate() {
        if states[index] == State::Pending
            && (stopped
                || node.needs.iter().any(|dependency| {
                    matches!(states[*dependency], State::Failed | State::Blocked)
                }))
        {
            states[index] = State::Blocked;
        }
    }
}

fn launchable(
    states: &[State],
    plan: &Plan,
    used_cpu: usize,
    used_memory: u64,
    continue_on_failure: bool,
) -> Option<usize> {
    if !continue_on_failure && states.contains(&State::Failed) {
        return None;
    }
    states.iter().enumerate().find_map(|(index, state)| {
        let node = &plan.nodes[index];
        (*state == State::Pending
            && node
                .needs
                .iter()
                .all(|dependency| states[*dependency] == State::Passed)
            && used_cpu
                .checked_add(node.cpu)
                .is_some_and(|cpu| cpu <= plan.cpu_slots)
            && plan.memory_mib.is_none_or(|budget| {
                used_memory
                    .checked_add(node.memory_mib)
                    .is_some_and(|memory| memory <= budget)
            })
            && states
                .iter()
                .filter(|state| **state == State::Running)
                .count()
                < plan.max_parallel)
            .then_some(index)
    })
}

fn run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}
