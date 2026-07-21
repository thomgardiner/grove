use anyhow::{Result, bail};
use std::path::Path;

use super::{Lifecycle, Task, Verification, lane_busy, load, now_secs, write};
use crate::claim;
use grove_core::task::reconcile;

struct Change {
    state: Lifecycle,
    reason: Option<String>,
    verification: Option<Verification>,
    verification_reason: Option<String>,
    source_sha256: Option<Option<String>>,
}

fn transition(root: &Path, repo: &str, id: &str, change: Change) -> Result<Task> {
    let task = {
        let _lock = claim::registry_lock(root, repo)?;
        transition_locked(root, repo, id, change)?
    };
    super::renew(root, &task);
    Ok(task)
}

fn transition_locked(root: &Path, repo: &str, id: &str, change: Change) -> Result<Task> {
    let Change {
        state,
        reason,
        verification,
        verification_reason,
        source_sha256,
    } = change;
    let mut task = load(root, repo, id)?;
    if task.lifecycle == state {
        if let Some(source_sha256) = source_sha256
            && task.source_sha256 != source_sha256
        {
            bail!("task {id} is already finished with a different source binding");
        }
        if let Some(verification) = verification
            && task.verification != verification
        {
            task.verification = verification;
            task.verification_reason = verification_reason;
            write(root, &task)?;
        }
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
    if let Some(verification) = verification {
        task.verification = verification;
        task.verification_reason = verification_reason;
    }
    if let Some(source_sha256) = source_sha256 {
        task.source_sha256 = source_sha256;
    }
    task.last_activity = now;
    write(root, &task)?;
    crate::events::record(
        root,
        repo,
        match state {
            Lifecycle::Finished => "task.finished",
            _ => "task.abandoned",
        },
        serde_json::json!({
            "task_id": task.id,
            "agent": task.agent,
            "verification": serde_json::to_value(task.verification).unwrap_or_default(),
        }),
    );
    Ok(task)
}

pub(crate) fn finish_with_verification_locked(
    root: &Path,
    repo: &str,
    id: &str,
    verification: Verification,
    verification_reason: Option<String>,
    source_sha256: Option<String>,
) -> Result<Task> {
    transition_locked(
        root,
        repo,
        id,
        Change {
            state: Lifecycle::Finished,
            reason: None,
            verification: Some(verification),
            verification_reason,
            source_sha256: Some(source_sha256),
        },
    )
}

pub fn abandon(root: &Path, repo: &str, id: &str, reason: String) -> Result<Task> {
    transition(
        root,
        repo,
        id,
        Change {
            state: Lifecycle::Abandoned,
            reason: Some(reason),
            verification: None,
            verification_reason: None,
            source_sha256: None,
        },
    )
}
