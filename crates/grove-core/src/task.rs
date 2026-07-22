//! Durable task records and generic process-liveness reconciliation.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::snapshot;

pub const SCHEMA_VERSION: u32 = 6;
/// Records this old and newer migrate forward; anything older fails closed
/// because its cleanup ownership cannot be established.
const OLDEST_SUPPORTED_SCHEMA_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Running,
    Recovering,
    Finished,
    Abandoned,
}
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CommandState {
    Starting,
    Running,
    Exited,
    Interrupted,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verification {
    Unverified,
    Overridden,
    Passed,
    Failed,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CommandRecord {
    pub argv: Vec<String>,
    pub pid: Option<u32>,
    pub process_start: Option<u64>,
    #[serde(default)]
    pub supervisor_pid: Option<u32>,
    #[serde(default)]
    pub supervisor_start: Option<u64>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub state: CommandState,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RecoveryRecord {
    pub attempted_at: u64,
    pub reason: String,
    pub error: Option<String>,
    pub saved_to: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Task {
    pub schema_version: u32,
    pub id: String,
    pub repo: String,
    pub agent: String,
    pub description: String,
    pub scope: Vec<String>,
    #[serde(default)]
    pub resolved_scope: Vec<String>,
    #[serde(default)]
    pub scope_snapshot: Option<snapshot::Ref>,
    #[serde(default)]
    pub claim_group: Option<String>,
    pub workspace: String,
    pub toolchain: String,
    pub branch: Option<String>,
    pub created_at: u64,
    pub last_activity: u64,
    pub lifecycle: Lifecycle,
    pub commands: Vec<CommandRecord>,
    pub reason: Option<String>,
    pub verification: Verification,
    #[serde(default)]
    pub verification_reason: Option<String>,
    /// Exact inspection source digest bound by the first terminal finish.
    #[serde(default)]
    pub source_sha256: Option<String>,
    /// Verification-policy digest pinned when the task began, so a candidate
    /// cannot weaken its own acceptance bar mid-task. Absent on legacy records.
    #[serde(default)]
    pub verification_policy_sha256: Option<String>,
    #[serde(default)]
    pub recovery: Option<RecoveryRecord>,
}

pub fn process_start(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(|process| process.start_time())
}

pub fn process_live(command: &CommandRecord) -> bool {
    matches!(
        (command.pid, command.process_start),
        (Some(pid), Some(start)) if process_start(pid) == Some(start)
    )
}

pub fn starting_pending(command: &CommandRecord) -> bool {
    command.state == CommandState::Starting
        && match (command.supervisor_pid, command.supervisor_start) {
            (Some(pid), Some(start)) => process_start(pid) == Some(start),
            _ => true,
        }
}

pub fn reconcile(task: &mut Task, now: u64, lane_held: bool) -> bool {
    let Some(command) = task.commands.last_mut() else {
        return false;
    };
    if !matches!(
        command.state,
        CommandState::Starting | CommandState::Running
    ) {
        return false;
    }
    if lane_held || process_live(command) || starting_pending(command) {
        return true;
    }
    command.state = CommandState::Interrupted;
    command.ended_at = Some(now);
    command.exit_code = Some(1);
    task.last_activity = now;
    false
}

fn dir(root: &Path, repo: &str) -> PathBuf {
    root.join("tasks").join(crate::repo_slug(repo))
}

fn path(root: &Path, repo: &str, id: &str) -> PathBuf {
    dir(root, repo).join(format!("{id}.json"))
}

pub fn write(root: &Path, task: &Task) -> Result<()> {
    crate::write_atomic(
        &path(root, &task.repo, &task.id),
        &serde_json::to_vec_pretty(task)?,
    )
}

/// Step every supported older record forward one version at a time, so adding a
/// version never silently drops the one before it. Absent fields are already
/// `None` from `serde(default)`; each step exists to move the version stamp and
/// to state which field the older record could not have carried.
fn migrate(mut task: Task, path: &Path) -> Result<Task> {
    if !(OLDEST_SUPPORTED_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&task.schema_version) {
        bail!(
            "task record {} has unknown cleanup ownership",
            path.display()
        );
    }
    if task.schema_version == 4 {
        task.source_sha256 = None;
        task.schema_version = 5;
    }
    if task.schema_version == 5 {
        // Begun before policy pinning: no digest to compare against, so finish
        // evaluates it exactly as it did before rather than refusing.
        task.verification_policy_sha256 = None;
        task.schema_version = 6;
    }
    Ok(task)
}

pub fn load(root: &Path, repo: &str, id: &str) -> Result<Task> {
    let path = path(root, repo, id);
    let bytes = fs::read(&path).with_context(|| format!("no task {id} in this repository"))?;
    let task =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    migrate(task, &path)
}

pub fn records_readonly(root: &Path, repo: &str) -> Result<Vec<Task>> {
    let entries = match fs::read_dir(dir(root, repo)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    entries
        .map(|entry| {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                return Ok(None);
            }
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let task = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "malformed task record {} preserved during read-only recovery",
                    path.display()
                )
            })?;
            let task = migrate(task, &path)?;
            if task.repo != repo {
                bail!(
                    "task record {} has unknown cleanup ownership",
                    path.display()
                )
            }
            Ok(Some(task))
        })
        .collect::<Result<Vec<_>>>()
        .map(|tasks| tasks.into_iter().flatten().collect())
}

pub fn blockers_except(
    root: &Path,
    repo: &str,
    workspace: &Path,
    ignore: Option<&str>,
) -> Result<Vec<String>> {
    let _guard = crate::claim::registry_lock(root, repo)?;
    let target = crate::canonical_path(workspace);
    let mut ids = Vec::new();
    for task in records_readonly(root, repo)? {
        if task.schema_version != SCHEMA_VERSION || task.repo != repo {
            bail!("task ownership is unknown")
        }
        if crate::canonical_path(Path::new(&task.workspace)) == target
            && matches!(task.lifecycle, Lifecycle::Running | Lifecycle::Recovering)
            && ignore != Some(task.id.as_str())
        {
            ids.push(task.id);
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}
