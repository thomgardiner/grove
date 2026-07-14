//! Conservative recovery of stale task records.
//!
//! A task enters `recovering` before Grove touches a leased worktree. That state keeps
//! its claim live, blocks new task commands, and leaves machine-readable evidence if
//! salvage fails. Only after work is safely preserved does the terminal record release
//! the claim.

use anyhow::{Result, bail};
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, claim, project, task, worktree};
use task::{CommandState, Lifecycle, RecoveryRecord, Task};

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
pub struct ReapedTask {
    pub id: String,
    pub workspace: String,
    pub saved_to: Option<String>,
    pub reason: String,
}

#[derive(Serialize)]
pub struct SkippedTask {
    pub id: String,
    pub workspace: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct ReapReport {
    pub schema_version: u32,
    pub dry_run: bool,
    pub reaped: Vec<ReapedTask>,
    pub skipped: Vec<SkippedTask>,
}

enum Candidate {
    Ready { task: Task, reason: String },
    Skipped { task: Task, reason: String },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn active_reason(root: &Path, task: &Task) -> Option<String> {
    if cache::workspace_busy(root, &task.workspace) {
        return Some("an active lane holds this workspace".to_string());
    }
    let command = task.commands.last()?;
    if !matches!(
        command.state,
        CommandState::Starting | CommandState::Running
    ) {
        return None;
    }
    if task::process_live(command) {
        return Some("supervised task process is still live".to_string());
    }
    // A supervisor can die immediately after spawning a child and before persisting its
    // PID. A lock cannot prove that child is gone, so automated recovery never removes
    // the associated worktree or releases this claim on that ambiguous state.
    if command.state == CommandState::Starting || command.pid.is_none() {
        return Some(
            "command startup was not fully supervised; preserving task and worktree".to_string(),
        );
    }
    None
}

fn candidate(root: &Path, repo: &str, id: &str, ttl: u64, record: bool) -> Result<Candidate> {
    let _lock = claim::registry_lock(root, repo)?;
    let mut task = task::load(root, repo, id)?;
    if matches!(task.lifecycle, Lifecycle::Finished | Lifecycle::Abandoned) {
        return Ok(Candidate::Skipped {
            task,
            reason: "task is already terminal".to_string(),
        });
    }
    if let Some(reason) = active_reason(root, &task) {
        return Ok(Candidate::Skipped { task, reason });
    }
    let idle = now_secs().saturating_sub(task.last_activity);
    if task.lifecycle == Lifecycle::Running && idle < ttl {
        return Ok(Candidate::Skipped {
            task,
            reason: format!("idle {idle}s, below ttl {ttl}s"),
        });
    }
    let reason = if task.lifecycle == Lifecycle::Recovering {
        task.recovery
            .as_ref()
            .map(|recovery| format!("retrying recovery: {}", recovery.reason))
            .unwrap_or_else(|| "retrying interrupted recovery".to_string())
    } else {
        format!("idle {idle}s")
    };
    if record {
        task.lifecycle = Lifecycle::Recovering;
        task.recovery = Some(RecoveryRecord {
            attempted_at: now_secs(),
            reason: reason.clone(),
            error: None,
            saved_to: None,
        });
        task::write(root, &task)?;
    }
    Ok(Candidate::Ready { task, reason })
}

fn recovery_failed(root: &Path, repo: &str, id: &str, error: String) -> Result<()> {
    let _lock = claim::registry_lock(root, repo)?;
    let mut task = task::load(root, repo, id)?;
    if task.lifecycle != Lifecycle::Recovering {
        bail!("task {id} left recovery before its failure could be recorded");
    }
    let recovery = task.recovery.get_or_insert(RecoveryRecord {
        attempted_at: now_secs(),
        reason: "recovery failure".to_string(),
        error: None,
        saved_to: None,
    });
    recovery.error = Some(error);
    task::write(root, &task)
}

fn recovery_finished(root: &Path, repo: &str, id: &str, saved_to: Option<String>) -> Result<Task> {
    let _lock = claim::registry_lock(root, repo)?;
    let mut task = task::load(root, repo, id)?;
    if task.lifecycle != Lifecycle::Recovering {
        bail!("task {id} left recovery before its terminal state could be recorded");
    }
    let recovery = task.recovery.get_or_insert(RecoveryRecord {
        attempted_at: now_secs(),
        reason: "stale task recovery".to_string(),
        error: None,
        saved_to: None,
    });
    recovery.error = None;
    recovery.saved_to = saved_to.clone();
    task.lifecycle = Lifecycle::Abandoned;
    task.reason = Some(format!("recovered: {}", recovery.reason));
    task.last_activity = now_secs();
    task::write(root, &task)?;
    Ok(task)
}

/// Reap task records idle past `ttl`. A task backed by a Grove lease reuses the existing
/// salvage-and-remove path; an ordinary checkout is never removed and its dirty state is
/// simply preserved before the task becomes abandoned. `dry_run` does not mutate records.
pub fn reap(root: &Path, workspace: &Path, ttl: u64, dry_run: bool) -> Result<ReapReport> {
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    let leased = worktree::list(root);
    let mut report = ReapReport {
        schema_version: SCHEMA_VERSION,
        dry_run,
        reaped: Vec::new(),
        skipped: Vec::new(),
    };
    for task in task::records(root, &repo)? {
        let candidate = candidate(root, &repo, &task.id, ttl, !dry_run)?;
        let (task, reason) = match candidate {
            Candidate::Ready { task, reason } => (task, reason),
            Candidate::Skipped { task, reason } => {
                if !matches!(task.lifecycle, Lifecycle::Finished | Lifecycle::Abandoned) {
                    report.skipped.push(SkippedTask {
                        id: task.id,
                        workspace: task.workspace,
                        reason,
                    });
                }
                continue;
            }
        };
        if dry_run {
            report.reaped.push(ReapedTask {
                id: task.id,
                workspace: task.workspace,
                saved_to: None,
                reason,
            });
            continue;
        }
        let outcome = leased
            .iter()
            .any(|lease| lease.path == task.workspace)
            .then(|| worktree::release(root, Path::new(&task.workspace)))
            .transpose();
        match outcome {
            Ok(release) => {
                let saved_to = release.and_then(|release| release.saved_to);
                let terminal = recovery_finished(root, &repo, &task.id, saved_to.clone())?;
                report.reaped.push(ReapedTask {
                    id: terminal.id,
                    workspace: terminal.workspace,
                    saved_to,
                    reason,
                });
            }
            Err(error) => {
                recovery_failed(root, &repo, &task.id, error.to_string())?;
                report.skipped.push(SkippedTask {
                    id: task.id,
                    workspace: task.workspace,
                    reason: format!("recovery blocked; claim and worktree retained: {error}"),
                });
            }
        }
    }
    Ok(report)
}
