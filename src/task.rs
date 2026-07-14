//! Durable task lifecycle built on Grove's existing claim registry and tagged lanes.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::api::Grove;
use crate::{cache, claim, git, project};

const SCHEMA_VERSION: u32 = 1;
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Lifecycle {
    Running,
    Finished,
    Abandoned,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CommandState {
    Starting,
    Running,
    Exited,
    Interrupted,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verification {
    Unverified,
    Passed,
    Failed,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct CommandRecord {
    pub(crate) argv: Vec<String>,
    pub(crate) pid: Option<u32>,
    pub(crate) process_start: Option<u64>,
    pub(crate) started_at: u64,
    pub(crate) ended_at: Option<u64>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) state: CommandState,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Task {
    pub(crate) schema_version: u32,
    pub(crate) id: String,
    pub(crate) repo: String,
    pub(crate) agent: String,
    pub(crate) description: String,
    pub(crate) scope: Vec<String>,
    pub(crate) workspace: String,
    pub(crate) toolchain: String,
    pub(crate) branch: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) last_activity: u64,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) commands: Vec<CommandRecord>,
    pub(crate) reason: Option<String>,
    pub(crate) verification: Verification,
}

pub struct Begin<'a> {
    pub root: &'a Path,
    pub workspace: &'a Path,
    pub agent: String,
    pub description: String,
    pub scope: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum BeginOutcome {
    Begun { task: Task },
    Conflict {
        requested: Vec<String>,
        conflicts: Vec<claim::Claim>,
    },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn task_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}

fn dir(root: &Path, repo: &str) -> PathBuf {
    root.join("tasks").join(cache::repo_slug(repo))
}

fn path(root: &Path, repo: &str, id: &str) -> PathBuf {
    dir(root, repo).join(format!("{id}.json"))
}

fn write(root: &Path, task: &Task) -> Result<()> {
    cache::write_atomic(
        &path(root, &task.repo, &task.id),
        &serde_json::to_vec_pretty(task)?,
    )
}

pub(crate) fn records(root: &Path, repo: &str) -> Result<Vec<Task>> {
    let Ok(entries) = fs::read_dir(dir(root, repo)) else {
        return Ok(Vec::new());
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        })
        .collect()
}

fn load(root: &Path, repo: &str, id: &str) -> Result<Task> {
    let path = path(root, repo, id);
    let bytes = fs::read(&path).with_context(|| format!("no task {id} in this repository"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

pub fn begin(req: Begin<'_>) -> Result<BeginOutcome> {
    let workspace = cache::canonical_path(req.workspace);
    let repo = project::repo_identity(&workspace);
    let _lock = claim::registry_lock(req.root, &repo)?;
    let conflicts = claim::conflicts_unlocked(req.root, &repo, &req.scope, None)?;
    if !conflicts.is_empty() {
        return Ok(BeginOutcome::Conflict {
            requested: req.scope,
            conflicts,
        });
    }
    let now = now_secs();
    let task = Task {
        schema_version: SCHEMA_VERSION,
        id: task_id(),
        branch: git::capture(&workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok(),
        toolchain: project::toolchain(&workspace),
        workspace: workspace.to_string_lossy().into_owned(),
        repo,
        agent: req.agent,
        description: req.description,
        scope: req.scope,
        created_at: now,
        last_activity: now,
        lifecycle: Lifecycle::Running,
        commands: Vec::new(),
        reason: None,
        verification: Verification::Unverified,
    };
    write(req.root, &task)?;
    Ok(BeginOutcome::Begun { task })
}

pub(crate) fn live_claims(root: &Path, repo: &str) -> Result<Vec<claim::Claim>> {
    Ok(records(root, repo)?
        .into_iter()
        .filter(|task| task.lifecycle == Lifecycle::Running)
        .map(|task| claim::Claim {
            id: task.id,
            agent: task.agent,
            task: task.description,
            scope: task.scope,
            branch: task.branch,
            created_at: task.created_at,
        })
        .collect())
}

fn tag(task: &Task) -> String {
    format!("task-{}", task.id)
}

fn process_start(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).map(|process| process.start_time())
}

fn process_live(command: &CommandRecord) -> bool {
    matches!(
        (command.pid, command.process_start),
        (Some(pid), Some(start)) if process_start(pid) == Some(start)
    )
}

fn lane_busy(root: &Path, task: &Task) -> bool {
    cache::tagged_busy(root, &task.workspace, &task.toolchain, &tag(task))
}

fn reconcile(task: &mut Task, now: u64, lane_held: bool) -> bool {
    let Some(command) = task.commands.last_mut() else {
        return false;
    };
    if !matches!(command.state, CommandState::Starting | CommandState::Running) {
        return false;
    }
    if lane_held || process_live(command) || command.state == CommandState::Starting {
        return true;
    }
    command.state = CommandState::Interrupted;
    command.ended_at = Some(now);
    command.exit_code = Some(1);
    task.last_activity = now;
    false
}

pub(crate) fn reconciled(root: &Path, repo: &str) -> Result<Vec<Task>> {
    let _lock = claim::registry_lock(root, repo)?;
    let mut tasks = records(root, repo)?;
    let now = now_secs();
    for task in &mut tasks {
        if task.lifecycle != Lifecycle::Running {
            continue;
        }
        let before = task.commands.last().map(|command| command.state);
        reconcile(task, now, lane_busy(root, task));
        if before != task.commands.last().map(|command| command.state) {
            write(root, task)?;
        }
    }
    Ok(tasks)
}

pub fn exec(root: &Path, repo: &str, id: &str, argv: &[String]) -> Result<i32> {
    let snapshot = load(root, repo, id)?;
    let grove = Grove::with_root(root.to_path_buf(), Path::new(&snapshot.workspace));
    let lane = grove.seeded_tagged_lane(&tag(&snapshot))?;
    let index = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        if task.lifecycle != Lifecycle::Running {
            bail!("task {id} is terminal");
        }
        if reconcile(&mut task, now_secs(), false) {
            bail!("task {id} already has a live command");
        }
        let index = task.commands.len();
        task.commands.push(CommandRecord {
            argv: argv.to_vec(),
            pid: None,
            process_start: None,
            started_at: now_secs(),
            ended_at: None,
            exit_code: None,
            state: CommandState::Starting,
        });
        task.last_activity = now_secs();
        write(root, &task)?;
        index
    };
    let (program, args) = argv.split_first().context("task exec requires a command")?;
    let mut command = Command::new(program);
    command.args(args).current_dir(&snapshot.workspace);
    cache::apply_env(&mut command, &lane);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            complete(root, repo, id, index, None, CommandState::Interrupted)?;
            return Err(error).with_context(|| format!("spawning {program}"));
        }
    };
    {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        let record = task.commands.get_mut(index).context("task command record disappeared")?;
        if let Some(start) = process_start(child.id()) {
            record.pid = Some(child.id());
            record.process_start = Some(start);
            record.state = CommandState::Running;
        }
        write(root, &task)?;
    }
    let status = child.wait().with_context(|| format!("waiting for {program}"))?;
    let state = if status.code().is_some() {
        CommandState::Exited
    } else {
        CommandState::Interrupted
    };
    complete(root, repo, id, index, status.code(), state)?;
    Ok(status.code().unwrap_or(1))
}

fn complete(
    root: &Path,
    repo: &str,
    id: &str,
    index: usize,
    code: Option<i32>,
    state: CommandState,
) -> Result<()> {
    let _lock = claim::registry_lock(root, repo)?;
    let mut task = load(root, repo, id)?;
    let record = task.commands.get_mut(index).context("task command record disappeared")?;
    record.state = state;
    record.exit_code = code.or(Some(1));
    record.ended_at = Some(now_secs());
    task.last_activity = now_secs();
    write(root, &task)
}

fn transition(root: &Path, repo: &str, id: &str, state: Lifecycle, reason: Option<String>) -> Result<Task> {
    let _lock = claim::registry_lock(root, repo)?;
    let mut task = load(root, repo, id)?;
    if task.lifecycle == state {
        return Ok(task);
    }
    if task.lifecycle != Lifecycle::Running {
        bail!("task {id} is already terminal");
    }
    let now = now_secs();
    let busy = lane_busy(root, &task);
    if reconcile(&mut task, now, busy) {
        bail!("task {id} still has a live command");
    }
    task.lifecycle = state;
    task.reason = reason;
    task.last_activity = now;
    write(root, &task)?;
    Ok(task)
}

pub fn finish(root: &Path, repo: &str, id: &str) -> Result<Task> {
    transition(root, repo, id, Lifecycle::Finished, None)
}

pub fn abandon(root: &Path, repo: &str, id: &str, reason: String) -> Result<Task> {
    transition(root, repo, id, Lifecycle::Abandoned, Some(reason))
}
