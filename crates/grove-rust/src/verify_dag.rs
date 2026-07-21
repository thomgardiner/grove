use anyhow::{Context, Result, bail};
use std::{sync::mpsc, thread};

use super::portable::PortableInputs;
use super::receipt::{ReceiptContext, complete_run, execute, now_nanos};
use super::{Receipt, VerifyReport, profile};
use crate::api::Grove;
use crate::{cache, config, task};

#[path = "verify_dag_context.rs"]
mod context;
#[path = "verify_dag_plan.rs"]
mod plan;
#[path = "verify_dag_schedule.rs"]
mod schedule;
use context::{run_id, task_context};
use plan::Plan;
use schedule::{State, block_dependents, launchable};

pub(super) fn validate(profile: &config::VerificationProfile) -> Result<()> {
    plan::validate(profile)
}

pub(super) fn profile_sha256(profile: &config::VerificationProfile) -> String {
    plan::profile_sha256(profile)
}

pub(super) fn run(
    root: &std::path::Path,
    workspace: &std::path::Path,
    config: &config::Config,
    name: &str,
    task_id: Option<&str>,
) -> Result<VerifyReport> {
    let (profile, required) = profile(config, name)?;
    let plan = Plan::new(&profile)?;
    let grove = Grove::bind(root.to_path_buf(), workspace.to_path_buf(), config.clone());
    grove.maintain(|| {
        if plan.legacy_serial() {
            let lane_tag = format!("verify-{name}");
            let commands = profile
                .commands
                .iter()
                .map(|command| command.argv.as_slice());
            let lane = Grove::commands(
                root.to_path_buf(),
                workspace.to_path_buf(),
                config.clone(),
                commands,
            )
            .seeded_tagged_lane(&lane_tag)?;
            serial(Serial {
                root,
                workspace,
                name,
                task_id,
                profile: &profile,
                required,
                lane_tag: &lane_tag,
                lane: &lane,
            })
        } else {
            parallel(&grove, name, task_id, &profile, required, &plan)
        }
    })
}

#[cfg(unix)]
pub(crate) fn run_locked_in_lane(
    root: &std::path::Path,
    workspace: &std::path::Path,
    config: &config::Config,
    name: &str,
    task_id: Option<&str>,
    lane_tag: &str,
    lane: &cache::Lane,
) -> Result<VerifyReport> {
    let (profile, required) = profile(config, name)?;
    let plan = Plan::new(&profile)?;
    if !plan.legacy_serial() {
        bail!("frozen release requires a serial verification profile")
    }
    serial(Serial {
        root,
        workspace,
        name,
        task_id,
        profile: &profile,
        required,
        lane_tag,
        lane,
    })
}

struct Serial<'a> {
    root: &'a std::path::Path,
    workspace: &'a std::path::Path,
    name: &'a str,
    task_id: Option<&'a str>,
    profile: &'a config::VerificationProfile,
    required: bool,
    lane_tag: &'a str,
    lane: &'a cache::Lane,
}

fn serial(input: Serial<'_>) -> Result<VerifyReport> {
    let (repo, task) = task_context(input.root, input.workspace, input.task_id)?;
    let run_id = run_id();
    let profile_sha256 = profile_sha256(input.profile);
    let portable = portable_inputs(input.workspace, input.profile, input.lane);
    let mut receipts = Vec::new();
    for (command_index, command) in input.profile.commands.iter().enumerate() {
        let context = ReceiptContext {
            root: input.root,
            workspace: input.workspace,
            repo: &repo,
            task: task.as_ref(),
            profile: input.name,
            run_id: &run_id,
            profile_sha256: &profile_sha256,
            command_index,
            required: input.required,
            lane_tag: input.lane_tag,
            lane: input.lane,
            portable: portable.as_ref(),
            portable_env: portable
                .as_ref()
                .map(|_| input.profile.portable_env.as_slice()),
        };
        let receipt = execute(
            &context,
            &command.argv,
            command.allow_zero_tests.unwrap_or(false),
        )?;
        let passed = receipt.passed;
        receipts.push(receipt);
        if !passed && !input.profile.continue_on_failure.unwrap_or(false) {
            break;
        }
    }
    finish(Finish {
        root: input.root,
        repo: &repo,
        task: task.as_ref(),
        name: input.name,
        run_id,
        profile_sha256,
        command_count: input.profile.commands.len(),
        receipts,
    })
}

fn parallel(
    grove: &Grove,
    name: &str,
    task_id: Option<&str>,
    profile: &config::VerificationProfile,
    required: bool,
    plan: &Plan,
) -> Result<VerifyReport> {
    let root = grove.root();
    let workspace = grove.workspace();
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
                    let result = worker(Worker {
                        grove,
                        repo: &repo,
                        task: task.as_ref(),
                        name,
                        profile,
                        run_id: &run_id,
                        profile_sha256: &profile_sha256,
                        command_index: index,
                        required,
                        lane_tag: &lane_tag,
                        argv: &argv,
                        allow_zero_tests,
                    });
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
    finish(Finish {
        root,
        repo: &repo,
        task: task.as_ref(),
        name,
        run_id,
        profile_sha256,
        command_count: count,
        receipts: receipts.into_iter().flatten().collect(),
    })
}

struct Worker<'a> {
    grove: &'a Grove,
    repo: &'a str,
    task: Option<&'a task::Task>,
    name: &'a str,
    profile: &'a config::VerificationProfile,
    run_id: &'a str,
    profile_sha256: &'a str,
    command_index: usize,
    required: bool,
    lane_tag: &'a str,
    argv: &'a [String],
    allow_zero_tests: bool,
}

fn worker(input: Worker<'_>) -> Result<Receipt> {
    let lane = Grove::command(
        input.grove.root().to_path_buf(),
        input.grove.workspace().to_path_buf(),
        input.grove.config().clone(),
        input.argv,
    )
    .seeded_tagged_lane(input.lane_tag)?;
    let portable = portable_inputs(input.grove.workspace(), input.profile, &lane);
    let context = ReceiptContext {
        root: input.grove.root(),
        workspace: input.grove.workspace(),
        repo: input.repo,
        task: input.task,
        profile: input.name,
        run_id: input.run_id,
        profile_sha256: input.profile_sha256,
        command_index: input.command_index,
        required: input.required,
        lane_tag: input.lane_tag,
        lane: &lane,
        portable: portable.as_ref(),
        portable_env: portable
            .as_ref()
            .map(|_| input.profile.portable_env.as_slice()),
    };
    execute(&context, input.argv, input.allow_zero_tests)
}

fn portable_inputs(
    workspace: &std::path::Path,
    profile: &config::VerificationProfile,
    lane: &cache::Lane,
) -> Option<PortableInputs> {
    match super::portable::capture(workspace, profile, lane.keep_debuginfo, lane.governor) {
        Ok(inputs) => inputs,
        Err(error) => {
            eprintln!("grove: portable receipt unavailable: {error:#}");
            None
        }
    }
}

struct Finish<'a> {
    root: &'a std::path::Path,
    repo: &'a str,
    task: Option<&'a task::Task>,
    name: &'a str,
    run_id: String,
    profile_sha256: String,
    command_count: usize,
    receipts: Vec<Receipt>,
}

fn finish(input: Finish<'_>) -> Result<VerifyReport> {
    let run = super::receipt::Run {
        schema_version: 1,
        repository: input.repo.into(),
        task_id: input.task.map(|task| task.id.clone()),
        profile: input.name.into(),
        run_id: input.run_id.clone(),
        profile_sha256: input.profile_sha256.clone(),
        command_count: input.command_count,
        receipt_count: input.receipts.len(),
        passed: input.receipts.len() == input.command_count
            && input.receipts.iter().all(|receipt| receipt.passed),
        completed_at_nanos: now_nanos(),
    };
    complete_run(input.root, input.repo, &run)?;
    Ok(VerifyReport {
        profile: input.name.to_string(),
        run_id: input.run_id,
        passed: run.passed,
        receipts: input.receipts,
    })
}
