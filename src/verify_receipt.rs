//! Durable receipt storage and execution for verification profiles.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::portable::PortableInputs;
use crate::{cache, git, impact, snapshot, task};

pub(super) const SCHEMA_VERSION: u32 = 4;
const TAIL_BYTES: usize = 8 * 1024;
static RECEIPT_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub head: Option<String>,
    pub changed_paths: Vec<String>,
    pub branch: Option<String>,
    pub workspace: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LaneIdentity {
    pub tag: String,
    pub path: String,
    #[serde(default)]
    pub policy_sha256: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Evidence {
    #[serde(flatten)]
    pub checkout: Checkout,
    pub input: snapshot::Ref,
    pub output: snapshot::Ref,
    /// Clean-checkout inputs that another clone may compare without relying on this
    /// receipt's local workspace, branch, task, or agent identity.
    #[serde(default)]
    pub portable: Option<PortableInputs>,
}

/// A bounded, machine-readable record of a command Grove ran. Tails are captured as
/// produced; Grove makes no claim that they have been redacted.
#[derive(Serialize, Deserialize, Clone)]
pub struct Receipt {
    pub schema_version: u32,
    pub repository: String,
    pub task_id: Option<String>,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub profile: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub profile_sha256: String,
    #[serde(default)]
    pub command_index: usize,
    pub required: bool,
    #[serde(flatten)]
    pub evidence: Option<Evidence>,
    pub lane: LaneIdentity,
    pub argv: Vec<String>,
    pub started_at: u64,
    pub ended_at: u64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub interrupted: bool,
    pub test_count: Option<u64>,
    pub passed: bool,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// A durable completion record binds command receipts into one ordered profile run.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Run {
    pub schema_version: u32,
    pub repository: String,
    pub task_id: Option<String>,
    pub profile: String,
    pub run_id: String,
    pub profile_sha256: String,
    pub command_count: usize,
    pub receipt_count: usize,
    pub passed: bool,
    pub completed_at_nanos: u128,
}

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

#[path = "verify_receipt_store.rs"]
mod store;
pub(super) use store::{StoredReceipt, StoredRun, all_receipts, all_runs};

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

fn receipts_dir(root: &Path, repo: &str) -> PathBuf {
    root.join("receipts").join(cache::repo_slug(repo))
}

fn runs_dir(root: &Path, repo: &str) -> PathBuf {
    root.join("verification-runs").join(cache::repo_slug(repo))
}

fn receipt_path(root: &Path, repo: &str) -> PathBuf {
    let seq = RECEIPT_SEQ.fetch_add(1, Ordering::Relaxed);
    receipts_dir(root, repo).join(format!(
        "{:x}-{:x}-{seq:x}.json",
        now_secs(),
        std::process::id()
    ))
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
        super::portable::configure_command(&mut command, names);
    } else {
        command.args(args);
        cache::apply_env(&mut command, context.lane);
        command.env_remove("GROVE_RELEASE_SIGNING_KEY");
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
        schema_version: SCHEMA_VERSION,
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
            portable: context.portable.cloned(),
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
    cache::write_atomic(
        &receipt_path(context.root, context.repo),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(receipt)
}

pub(super) fn receipts(root: &Path, repo: &str) -> Result<Vec<Receipt>> {
    let Ok(entries) = fs::read_dir(receipts_dir(root, repo)) else {
        return Ok(Vec::new());
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| {
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        })
        .collect()
}

pub(super) fn complete_run(root: &Path, repo: &str, run: &Run) -> Result<()> {
    if run.run_id.is_empty()
        || !run
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        bail!("invalid verification run id")
    }
    cache::write_atomic(
        &runs_dir(root, repo).join(format!("{}.json", run.run_id)),
        &serde_json::to_vec_pretty(run)?,
    )
}

pub(super) fn runs(root: &Path, repo: &str) -> Result<Vec<Run>> {
    let Ok(entries) = fs::read_dir(runs_dir(root, repo)) else {
        return Ok(Vec::new());
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| {
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        })
        .collect()
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
