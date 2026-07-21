//! Leased inspection lifecycle built on exact standalone snapshots.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, inspection_process, inspection_snapshot, project, task};

#[path = "inspection_readonly.rs"]
mod readonly;
#[path = "inspection_state.rs"]
mod state;
pub use state::{AcquireReport, ExecReport, ReapReport, ReleaseReport};
use state::{
    Execution, Log, State, capsule_dir, live, lock, read_state, write_state, write_terminal,
};

pub const SCHEMA_VERSION: u32 = 1;
const MAX_TTL_SECS: u64 = 86_400;

pub fn acquire(
    root: &Path,
    workspace: &Path,
    task_id: &str,
    ttl_secs: u64,
) -> Result<AcquireReport> {
    if ttl_secs == 0 || ttl_secs > MAX_TTL_SECS {
        bail!("inspection lease TTL must be between 1 and {MAX_TTL_SECS} seconds")
    }
    let workspace = fs::canonicalize(workspace)?;
    let repo = project::repo_identity(&workspace);
    let task = task::load(root, &repo, task_id)?;
    if cache::canonical_path(Path::new(&task.workspace)) != workspace {
        bail!("task {task_id} belongs to another workspace")
    }
    if task.lifecycle != task::Lifecycle::Running {
        bail!("task {task_id} is not running")
    }
    let capsule_id = id(&workspace, task_id);
    let request = inspection_snapshot::Request {
        root,
        workspace: &workspace,
        task_id,
        capsule_id: &capsule_id,
        expires_at: now().saturating_add(ttl_secs),
    };
    let capsule = inspection_snapshot::acquire(&request)?;
    let execution = Execution {
        schema_version: SCHEMA_VERSION,
        capsule_id: capsule_id.clone(),
        task_id: task_id.to_string(),
        state: State::Ready,
        tree_pid: None,
        tree_start: None,
        started_at: None,
        ended_at: None,
        exit_code: None,
        source_sha256: capsule.binding.source_sha256.clone(),
        capsule_sha256: None,
    };
    if let Err(error) = write_state(&capsule, &execution) {
        return match inspection_snapshot::remove(&capsule) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "also failed to remove unbound inspection capsule: {cleanup:#}"
            ))),
        };
    }
    Ok(AcquireReport {
        schema_version: SCHEMA_VERSION,
        capsule_id,
        path: capsule.path,
        task_id: task_id.to_string(),
        source_sha256: capsule.binding.source_sha256,
        expires_at: capsule.binding.expires_at,
    })
}

pub fn exec(
    root: &Path,
    workspace: &Path,
    capsule_id: &str,
    argv: &[String],
    timeout_secs: Option<u64>,
) -> Result<ExecReport> {
    let capsule = inspection_snapshot::open(root, workspace, capsule_id)?;
    let _lock = lock(&capsule, false)?;
    let state = read_state(&capsule)?;
    if state.state != State::Ready {
        bail!("inspection capsule {capsule_id} has already been executed")
    }
    require_task(root, workspace, &capsule)?;
    let source_before = inspection_snapshot::digest(workspace)?;
    let capsule_before = inspection_snapshot::digest(&capsule.path)?;
    if source_before != capsule.binding.source_sha256 || capsule_before != source_before {
        bail!("inspection source or capsule changed before launch")
    }
    readonly::seal(&capsule.path)?;
    // Windows represents its portable read-only bit in the snapshot mode. Bind
    // post-exec integrity to the sealed baseline after first proving exactness.
    let capsule_baseline = inspection_snapshot::digest(&capsule.path)?;
    let dir = capsule_dir(&capsule)?;
    let stdout_path = dir.join("stdout.log");
    let stderr_path = dir.join("stderr.log");
    let stdout = create_log(&stdout_path)?;
    let stderr = create_log(&stderr_path)?;
    let runtime = dir.join("runtime");
    let result = inspection_process::run(
        &capsule.path,
        &runtime,
        argv,
        timeout_secs,
        &stdout,
        &stderr,
        |pid| {
            let running = Execution {
                schema_version: SCHEMA_VERSION,
                capsule_id: capsule_id.to_string(),
                task_id: capsule.binding.task_id.clone(),
                state: State::Running,
                tree_pid: Some(pid),
                tree_start: grove_core::task::process_start(pid),
                started_at: Some(now()),
                ended_at: None,
                exit_code: None,
                source_sha256: source_before.clone(),
                capsule_sha256: Some(capsule_baseline.clone()),
            };
            write_state(&capsule, &running)
        },
    );
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            let current = read_state(&capsule).unwrap_or(state);
            let _ = write_terminal(&capsule, &current, State::Invalid, 1, None);
            return Err(error);
        }
    };
    let running = read_state(&capsule)?;
    if !outcome.tree_clean {
        write_terminal(&capsule, &running, State::Invalid, outcome.exit_code, None)?;
        bail!("inspection process tree did not terminate")
    }
    let source_after = inspection_snapshot::digest(workspace)?;
    let capsule_after = inspection_snapshot::digest(&capsule.path).ok();
    let source_unchanged = source_after == source_before;
    let capsule_unchanged = capsule_after.as_deref() == Some(capsule_baseline.as_str());
    let output_within_limit = !outcome.stdout_truncated && !outcome.stderr_truncated;
    let authorized = source_unchanged
        && capsule_unchanged
        && output_within_limit
        && outcome.exit_code == 0
        && !outcome.timed_out;
    write_terminal(
        &capsule,
        &running,
        if authorized {
            State::Complete
        } else {
            State::Invalid
        },
        outcome.exit_code,
        capsule_after.clone(),
    )?;
    drop(stdout);
    drop(stderr);
    Ok(ExecReport {
        schema_version: SCHEMA_VERSION,
        capsule_id: capsule_id.to_string(),
        task_id: capsule.binding.task_id,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        tree_clean: outcome.tree_clean,
        source_unchanged,
        capsule_unchanged,
        authorized,
        source_sha256: source_after,
        capsule_sha256: capsule_after,
        stdout: log(&stdout_path, outcome.stdout_truncated)?,
        stderr: log(&stderr_path, outcome.stderr_truncated)?,
    })
}

pub fn release(root: &Path, workspace: &Path, capsule_id: &str) -> Result<ReleaseReport> {
    let capsule = inspection_snapshot::open_for_cleanup(root, workspace, capsule_id)?;
    let _lock = lock(&capsule, true)?;
    let state = read_state(&capsule)?;
    if state.state == State::Running && live(&state) {
        bail!("inspection capsule {capsule_id} is still running")
    }
    cleanup_tree(&state)?;
    readonly::unseal(&capsule.path)?;
    inspection_snapshot::remove(&capsule)?;
    Ok(ReleaseReport {
        schema_version: SCHEMA_VERSION,
        capsule_id: capsule_id.to_string(),
        released: true,
    })
}

pub fn reap(root: &Path, workspace: &Path, dry_run: bool) -> Result<ReapReport> {
    let mut report = ReapReport {
        schema_version: SCHEMA_VERSION,
        dry_run,
        reaped: Vec::new(),
        kept: Vec::new(),
        errors: Vec::new(),
    };
    for id in inspection_snapshot::list(root, workspace)? {
        match reap_one(root, workspace, &id, dry_run) {
            Ok(true) => report.reaped.push(id),
            Ok(false) => report.kept.push(id),
            Err(error) => report.errors.push(format!("{id}: {error:#}")),
        }
    }
    Ok(report)
}

fn reap_one(root: &Path, workspace: &Path, id: &str, dry_run: bool) -> Result<bool> {
    let capsule = inspection_snapshot::open_for_cleanup(root, workspace, id)?;
    if capsule.binding.expires_at > now() {
        return Ok(false);
    }
    let _lock = match lock(&capsule, true) {
        Ok(lock) => lock,
        Err(_) => return Ok(false),
    };
    let state = read_state(&capsule)?;
    if live(&state) {
        return Ok(false);
    }
    if !dry_run {
        cleanup_tree(&state)?;
        readonly::unseal(&capsule.path)?;
        inspection_snapshot::remove(&capsule)?;
    }
    Ok(true)
}

fn require_task(
    root: &Path,
    workspace: &Path,
    capsule: &inspection_snapshot::Capsule,
) -> Result<()> {
    let repo = project::repo_identity(workspace);
    let task = task::load(root, &repo, &capsule.binding.task_id)?;
    if cache::canonical_path(Path::new(&task.workspace)) != cache::canonical_path(workspace) {
        bail!("inspection task belongs to another workspace")
    }
    if task.lifecycle != task::Lifecycle::Running {
        bail!("inspection task {} is not running", capsule.binding.task_id)
    }
    Ok(())
}

fn create_log(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(Into::into)
}

fn log(path: &Path, truncated: bool) -> Result<Log> {
    let bytes = fs::read(path)?;
    Ok(Log {
        path: path.to_path_buf(),
        sha256: crate::hex(&Sha256::digest(&bytes)),
        bytes: bytes.len() as u64,
        truncated,
    })
}

#[cfg(unix)]
fn cleanup_tree(state: &Execution) -> Result<()> {
    if let Some(pid) = state.tree_pid {
        let group = pid as libc::pid_t;
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let live = unsafe { libc::killpg(group, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !live {
                break;
            }
            if std::time::Instant::now() >= deadline {
                bail!("inspection process group remained live during cleanup")
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn cleanup_tree(_state: &Execution) -> Result<()> {
    Ok(())
}

fn id(workspace: &Path, task: &str) -> String {
    let nonce = format!(
        "{}:{}:{}:{}",
        workspace.display(),
        task,
        now_nanos(),
        std::process::id()
    );
    crate::hex(&Sha256::digest(nonce.as_bytes()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}
