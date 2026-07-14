//! Repository-scoped view joining tasks, claims, worktrees, and build lanes.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

use crate::task::{CommandState, Lifecycle, Task};
use crate::{cache, claim, git, project, task, worktree};

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
        Lifecycle::Running => {}
    }
    if let Some(command) = task.commands.last() {
        match command.state {
            CommandState::Running => return TaskStatus::Active,
            CommandState::Starting if cache::tagged_busy(root, &task.workspace, &task.toolchain, &format!("task-{}", task.id)) => {
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
    let claims = claim::status(root, &repo)
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
    let task_ids: HashSet<&str> = report.tasks.iter().map(|task| task.task.id.as_str()).collect();
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
