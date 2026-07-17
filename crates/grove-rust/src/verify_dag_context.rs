use anyhow::{Result, bail};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{project, task};

pub(super) fn task_context(
    root: &Path,
    workspace: &Path,
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

pub(super) fn run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}
