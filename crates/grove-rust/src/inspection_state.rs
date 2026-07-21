use super::*;
use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Serialize)]
pub struct AcquireReport {
    pub(super) schema_version: u32,
    pub(super) capsule_id: String,
    pub(super) path: PathBuf,
    pub(super) task_id: String,
    pub(super) source_sha256: String,
    pub(super) expires_at: u64,
}

#[derive(Serialize)]
pub struct ExecReport {
    pub(super) schema_version: u32,
    pub(super) capsule_id: String,
    pub(super) task_id: String,
    pub(super) exit_code: i32,
    pub(super) timed_out: bool,
    pub(super) tree_clean: bool,
    pub(super) source_unchanged: bool,
    pub(super) capsule_unchanged: bool,
    pub(super) authorized: bool,
    pub(super) source_sha256: String,
    pub(super) capsule_sha256: Option<String>,
    pub(super) stdout: Log,
    pub(super) stderr: Log,
}

impl ExecReport {
    pub fn domain_exit(&self) -> i32 {
        if self.authorized { self.exit_code } else { 1 }
    }
}

#[derive(Serialize)]
pub(super) struct Log {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
    pub(super) bytes: u64,
    pub(super) truncated: bool,
}

#[derive(Serialize)]
pub struct ReleaseReport {
    pub(super) schema_version: u32,
    pub(super) capsule_id: String,
    pub(super) released: bool,
}

#[derive(Serialize)]
pub struct ReapReport {
    pub(super) schema_version: u32,
    pub(super) dry_run: bool,
    pub(super) reaped: Vec<String>,
    pub(super) kept: Vec<String>,
    pub(super) errors: Vec<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Ready,
    Running,
    Complete,
    Invalid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Execution {
    pub(super) schema_version: u32,
    pub(super) capsule_id: String,
    pub(super) task_id: String,
    pub(super) state: State,
    pub(super) tree_pid: Option<u32>,
    pub(super) tree_start: Option<u64>,
    pub(super) started_at: Option<u64>,
    pub(super) ended_at: Option<u64>,
    pub(super) exit_code: Option<i32>,
    pub(super) source_sha256: String,
    pub(super) capsule_sha256: Option<String>,
}

pub(super) fn lock(capsule: &inspection_snapshot::Capsule, nonblocking: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(capsule_dir(capsule)?.join("exec.lock"))?;
    if nonblocking {
        file.try_lock_exclusive()
            .context("inspection capsule is busy")?;
    } else {
        file.lock_exclusive()?;
    }
    Ok(file)
}

pub(super) fn capsule_dir(capsule: &inspection_snapshot::Capsule) -> Result<&Path> {
    capsule
        .path
        .parent()
        .context("inspection capsule has no directory")
}

pub(super) fn write_state(capsule: &inspection_snapshot::Capsule, state: &Execution) -> Result<()> {
    cache::write_atomic(
        &capsule_dir(capsule)?.join("execution.json"),
        &serde_json::to_vec_pretty(state)?,
    )
}

pub(super) fn read_state(capsule: &inspection_snapshot::Capsule) -> Result<Execution> {
    let path = capsule_dir(capsule)?.join("execution.json");
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_STATE_BYTES
    {
        bail!("inspection execution state is not a bounded regular file")
    }
    let state: Execution = serde_json::from_slice(&fs::read(path)?)?;
    if state.schema_version != SCHEMA_VERSION
        || state.capsule_id != capsule.binding.capsule_id
        || state.task_id != capsule.binding.task_id
        || state.source_sha256 != capsule.binding.source_sha256
    {
        bail!("inspection execution state does not match its binding")
    }
    Ok(state)
}

pub(super) fn write_terminal(
    capsule: &inspection_snapshot::Capsule,
    previous: &Execution,
    state: State,
    code: i32,
    digest: Option<String>,
) -> Result<()> {
    write_state(
        capsule,
        &Execution {
            schema_version: SCHEMA_VERSION,
            capsule_id: previous.capsule_id.clone(),
            task_id: previous.task_id.clone(),
            state,
            tree_pid: previous.tree_pid,
            tree_start: previous.tree_start,
            started_at: previous.started_at.or(Some(now())),
            ended_at: Some(now()),
            exit_code: Some(code),
            source_sha256: previous.source_sha256.clone(),
            capsule_sha256: digest,
        },
    )
}

pub(super) fn live(state: &Execution) -> bool {
    matches!((state.tree_pid, state.tree_start), (Some(pid), Some(start)) if grove_core::task::process_start(pid) == Some(start))
}
