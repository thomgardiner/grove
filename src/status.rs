//! Repository-scoped view joining tasks, claims, worktrees, and build lanes.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

use crate::task::{CommandState, Lifecycle, Task};
use crate::{cache, claim, git, project, task, verify, worktree};

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
enum TaskStatus {
    Active,
    Idle,
    Stalled,
    Failed,
    Finished,
    Abandoned,
}

#[derive(Serialize)]
struct TaskView {
    #[serde(flatten)]
    task: Task,
    status: TaskStatus,
    elapsed_secs: u64,
    dirty: bool,
}

#[derive(Serialize)]
pub struct Report {
    schema_version: u32,
    repository: String,
    tasks: Vec<TaskView>,
    claims: Vec<claim::Claim>,
    worktrees: Vec<worktree::WorktreeInfo>,
    lanes: Vec<cache::LaneStatus>,
}

/// The compact task view intended for an agent swarm, rather than the full repository
/// board emitted by `grove status`.
#[derive(Serialize)]
pub struct TaskReport {
    schema_version: u32,
    repository: String,
    tasks: Vec<TaskDetail>,
}

#[derive(Serialize)]
struct TaskDetail {
    id: String,
    owner: String,
    task: String,
    scope: Vec<String>,
    resolved_scope: Vec<String>,
    status: TaskStatus,
    heartbeat_at: u64,
    heartbeat_age_secs: u64,
    active_command: Option<ActiveCommand>,
    verification: Freshness,
    conflicts: Vec<claim::Claim>,
}

#[derive(Serialize)]
struct ActiveCommand {
    argv: Vec<String>,
    pid: Option<u32>,
    state: &'static str,
}

#[derive(Serialize)]
struct Freshness {
    state: &'static str,
    required: Vec<String>,
    passed: Vec<String>,
    missing: Vec<String>,
    stale: Vec<String>,
    failed: Vec<String>,
    error: Option<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn state(root: &Path, task: &Task, now: u64) -> TaskStatus {
    match task.lifecycle {
        Lifecycle::Finished => return TaskStatus::Finished,
        Lifecycle::Abandoned => return TaskStatus::Abandoned,
        // A stale-task recovery holds the claim until salvage succeeds or reports its
        // failure in the task record, so it is deliberately visible as stalled.
        Lifecycle::Recovering => return TaskStatus::Stalled,
        Lifecycle::Running => {}
    }
    if let Some(command) = task.commands.last() {
        match command.state {
            CommandState::Running => return TaskStatus::Active,
            CommandState::Starting
                if cache::tagged_busy(
                    root,
                    &task.workspace,
                    &task.toolchain,
                    &format!("task-{}", task.id),
                ) =>
            {
                return TaskStatus::Active;
            }
            CommandState::Starting => return TaskStatus::Stalled,
            CommandState::Interrupted => return TaskStatus::Failed,
            CommandState::Exited if command.exit_code != Some(0) => return TaskStatus::Failed,
            CommandState::Exited => {}
        }
    }
    if now.saturating_sub(task.last_activity) > claim::claim_ttl() {
        TaskStatus::Stalled
    } else {
        TaskStatus::Idle
    }
}

fn dirty(task: &Task) -> bool {
    git::capture(Path::new(&task.workspace), &["status", "--porcelain"])
        .map(|output| !output.is_empty())
        .unwrap_or(false)
}

pub fn report(root: &Path, workspace: &Path) -> Result<Report> {
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    let tasks = task::reconciled(root, &repo)?;
    let now = now_secs();
    let views = tasks
        .iter()
        .cloned()
        .map(|task| TaskView {
            elapsed_secs: now.saturating_sub(task.created_at),
            status: state(root, &task, now),
            dirty: dirty(&task),
            task,
        })
        .collect();
    let worktrees: Vec<_> = worktree::list(root)
        .into_iter()
        .filter(|worktree| worktree.repo == repo)
        .collect();
    let mut workspaces: HashSet<String> = worktrees
        .iter()
        .map(|worktree| worktree.path.clone())
        .collect();
    workspaces.insert(workspace.to_string_lossy().into_owned());
    workspaces.extend(tasks.iter().map(|task| task.workspace.clone()));
    let lanes = cache::status(root)
        .lanes
        .into_iter()
        .filter(|lane| {
            lane.workspace
                .as_ref()
                .is_some_and(|workspace| workspaces.contains(workspace))
        })
        .collect();
    let task_ids: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let claims = claim::status(root, &repo)?
        .into_iter()
        .filter(|claim| !task_ids.contains(claim.id.as_str()))
        .collect();
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        repository: repo.clone(),
        tasks: views,
        claims,
        worktrees,
        lanes,
    })
}

/// Show one task by ID, every task, or only durable live tasks with `active`.
pub fn task_report(
    root: &Path,
    workspace: &Path,
    id: Option<&str>,
    active: bool,
) -> Result<TaskReport> {
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    let now = now_secs();
    let tasks = task::reconciled(root, &repo)?;
    let mut details = Vec::new();
    for task in tasks {
        if id.is_some_and(|id| task.id != id) {
            continue;
        }
        if active && !matches!(task.lifecycle, Lifecycle::Running | Lifecycle::Recovering) {
            continue;
        }
        let scope = if task.resolved_scope.is_empty() {
            task.scope.clone()
        } else {
            task.resolved_scope.clone()
        };
        let conflicts = claim::conflicts(root, &repo, &workspace, &scope, &task.id)?;
        details.push(TaskDetail {
            id: task.id.clone(),
            owner: task.agent.clone(),
            task: task.description.clone(),
            scope: task.scope.clone(),
            resolved_scope: task.resolved_scope.clone(),
            status: state(root, &task, now),
            heartbeat_at: task.last_activity,
            heartbeat_age_secs: now.saturating_sub(task.last_activity),
            active_command: active_command(&task),
            verification: freshness(root, &repo, &task.id),
            conflicts,
        });
    }
    if id.is_some() && details.is_empty() {
        anyhow::bail!("no task {id:?} in this repository");
    }
    Ok(TaskReport {
        schema_version: SCHEMA_VERSION,
        repository: repo,
        tasks: details,
    })
}

fn active_command(task: &Task) -> Option<ActiveCommand> {
    let command = task.commands.last()?;
    let state = match command.state {
        CommandState::Starting => "starting",
        CommandState::Running => "running",
        CommandState::Exited | CommandState::Interrupted => return None,
    };
    Some(ActiveCommand {
        argv: command.argv.clone(),
        pid: command.pid,
        state,
    })
}

fn freshness(root: &Path, repo: &str, id: &str) -> Freshness {
    match verify::task_verification(root, repo, id) {
        Ok(verification) => {
            let state = if verification.verified {
                "fresh"
            } else if !verification.stale.is_empty() {
                "stale"
            } else if !verification.failed.is_empty() {
                "failed"
            } else if !verification.missing.is_empty() {
                "missing"
            } else {
                "unverified"
            };
            Freshness {
                state,
                required: verification.required,
                passed: verification.passed,
                missing: verification.missing,
                stale: verification.stale,
                failed: verification.failed,
                error: None,
            }
        }
        Err(error) => Freshness {
            state: "invalid",
            required: Vec::new(),
            passed: Vec::new(),
            missing: Vec::new(),
            stale: Vec::new(),
            failed: Vec::new(),
            error: Some(format!("{error:#}")),
        },
    }
}

pub fn print_tasks(report: &TaskReport) {
    if report.tasks.is_empty() {
        println!("no matching tasks");
        return;
    }
    for task in &report.tasks {
        let command = task
            .active_command
            .as_ref()
            .map(|command| command.argv.join(" "))
            .unwrap_or_else(|| "-".into());
        println!(
            "{} owner={} status={:?} heartbeat={}s verification={} command={} scope=[{}] conflicts={}",
            task.id,
            task.owner,
            task.status,
            task.heartbeat_age_secs,
            task.verification.state,
            command,
            task.scope.join(", "),
            task.conflicts.len(),
        );
    }
}

pub fn print(report: &Report) {
    if report.tasks.is_empty() && report.claims.is_empty() {
        println!("no active tasks or claims");
        return;
    }
    for task in &report.tasks {
        let pid = task
            .task
            .commands
            .last()
            .and_then(|command| command.pid)
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<10} {:<12} {:<12} pid={:<8} {} [{}]",
            task.task.id,
            format!("{:?}", task.status).to_lowercase(),
            task.task.agent,
            pid,
            task.task.description,
            task.task.scope.join(", ")
        );
    }
    let task_ids: HashSet<&str> = report
        .tasks
        .iter()
        .map(|task| task.task.id.as_str())
        .collect();
    for claim in &report.claims {
        if task_ids.contains(claim.id.as_str()) {
            continue;
        }
        println!(
            "claim      {:<12} {:<24} [{}]",
            claim.agent,
            claim.task,
            claim.scope.join(", ")
        );
    }
}
