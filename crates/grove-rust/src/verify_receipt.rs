//! Durable receipt storage and execution for verification profiles.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::portable::PortableInputs;
use crate::{cache, git, impact, snapshot, task};
const TAIL_BYTES: usize = 8 * 1024;
pub(super) use grove_core::verification::{Checkout, Evidence, LaneIdentity, Receipt, Run};

pub(super) struct ReceiptContext<'a> {
    pub(super) root: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) repo: &'a str,
    pub(super) task: Option<&'a task::Task>,
    pub(super) profile: &'a str,
    pub(super) run_id: &'a str,
    pub(super) profile_sha256: &'a str,
    pub(super) command_index: usize,
    pub(super) required: bool,
    pub(super) lane_tag: &'a str,
    pub(super) lane: &'a cache::Lane,
    pub(super) portable: Option<&'a PortableInputs>,
    pub(super) portable_env: Option<&'a [String]>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(super) fn checkout(workspace: &Path) -> Checkout {
    Checkout {
        head: git::capture(workspace, &["rev-parse", "HEAD"]).ok(),
        changed_paths: impact::changed_files(workspace, "HEAD").unwrap_or_default(),
        branch: git::capture(workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok(),
        workspace: cache::canonical_path(workspace)
            .to_string_lossy()
            .into_owned(),
    }
}

fn bounded_tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]).into_owned()
}

fn test_count(output: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(output);
    for line in text.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        for window in words.windows(3) {
            if window[1] == "tests"
                && window[2].trim_end_matches(':') == "run"
                && let Ok(count) = window[0].parse()
            {
                return Some(count);
            }
        }
        for window in words.windows(3) {
            if window[0] == "running"
                && window[2].starts_with("test")
                && let Ok(count) = window[1].parse()
            {
                return Some(count);
            }
        }
    }
    None
}

fn is_test_runner(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == "nextest" || arg == "test")
}

fn explicitly_selected_tests(argv: &[String]) -> bool {
    argv.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-p" | "--package"
                | "--test"
                | "--lib"
                | "--bin"
                | "--example"
                | "--bench"
                | "-E"
                | "--filter-expr"
                | "--exact"
        )
    })
}

/// Keep structured report output parseable: profile command streams are diagnostics,
/// while stdout is reserved for the final JSON receipt report.
fn emit(output: &[u8]) {
    let _ = std::io::stderr().write_all(output);
}

pub(super) fn execute(
    context: &ReceiptContext<'_>,
    argv: &[String],
    allow_zero_tests: bool,
) -> Result<Receipt> {
    let started_at = now_secs();
    let started = Instant::now();
    let state = checkout(context.workspace);
    let input = snapshot::capture(context.workspace)?;
    let input = snapshot::persist(context.root, context.repo, &input)?;
    let (program, args) = argv
        .split_first()
        .context("verification command has no argv")?;
    let mut command = Command::new(program);
    if let Some(names) = context.portable_env {
        command.args(super::portable::command_args(argv, context.lane));
        super::portable::configure_command(&mut command, names, context.lane);
    } else {
        command.args(args);
        cache::apply_env(&mut command, context.lane);
    }
    command.current_dir(context.workspace);
    let output = command.output();
    let (exit_code, interrupted, stdout, stderr) = match output {
        Ok(output) => (
            output.status.code(),
            output.status.code().is_none(),
            output.stdout,
            output.stderr,
        ),
        Err(error) => (None, true, Vec::new(), error.to_string().into_bytes()),
    };
    emit(&stdout);
    emit(&stderr);
    let mut combined = stdout.clone();
    combined.extend_from_slice(&stderr);
    let count = test_count(&combined);
    let zero_selected = is_test_runner(argv)
        && explicitly_selected_tests(argv)
        && count == Some(0)
        && !allow_zero_tests;
    if zero_selected {
        eprintln!("grove: selected test command ran zero tests; refusing a successful receipt");
    }
    let output = snapshot::capture(context.workspace)?;
    let output = snapshot::persist(context.root, context.repo, &output)?;
    let unchanged = input == output;
    if !unchanged {
        eprintln!(
            "grove: verification command changed workspace content; refusing a successful receipt"
        );
    }
    let passed = exit_code == Some(0) && !interrupted && !zero_selected && unchanged;
    let receipt = Receipt {
        schema_version: grove_core::verification::RECEIPT_SCHEMA_VERSION,
        repository: context.repo.to_string(),
        task_id: context.task.map(|task| task.id.clone()),
        agent: context.task.map(|task| task.agent.clone()),
        task: context.task.map(|task| task.description.clone()),
        profile: context.profile.to_string(),
        run_id: context.run_id.to_string(),
        profile_sha256: context.profile_sha256.to_string(),
        command_index: context.command_index,
        required: context.required,
        evidence: Some(Evidence {
            checkout: state,
            input,
            output,
            portable: context.portable.map(serde_json::to_value).transpose()?,
        }),
        lane: LaneIdentity {
            tag: context.lane_tag.to_string(),
            path: context.lane.dir.to_string_lossy().into_owned(),
            policy_sha256: context.lane.policy_sha256.clone(),
        },
        argv: argv.to_vec(),
        started_at,
        ended_at: now_secs(),
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        exit_code,
        interrupted,
        test_count: count,
        passed,
        stdout_tail: bounded_tail(&stdout),
        stderr_tail: bounded_tail(&stderr),
    };
    grove_core::verification::write_receipt(context.root, context.repo, &receipt)?;
    Ok(receipt)
}

pub(super) fn receipts(root: &Path, repo: &str) -> Result<Vec<Receipt>> {
    grove_core::verification::receipts(root, repo)
}

pub(super) fn complete_run(root: &Path, repo: &str, run: &Run) -> Result<()> {
    grove_core::verification::complete_run(root, repo, run)
}

pub(super) fn runs(root: &Path, repo: &str) -> Result<Vec<Run>> {
    grove_core::verification::runs(root, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nextest_and_cargo_test_counts() {
        assert_eq!(test_count(b"Summary: 12 tests run: 12 passed"), Some(12));
        assert_eq!(test_count(b"running 3 tests"), Some(3));
    }

    #[test]
    fn recognizes_explicit_test_selection() {
        assert!(explicitly_selected_tests(&[
            "cargo".into(),
            "test".into(),
            "--test".into(),
            "x".into()
        ]));
        assert!(!explicitly_selected_tests(&[
            "cargo".into(),
            "test".into(),
            "--workspace".into()
        ]));
    }
}
