use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::{CommandRecord, CommandState, Lifecycle, Task, load, now_secs, records, write};
use crate::api::Grove;
use crate::{cache, claim, worktree};
use grove_core::task::{process_start, reconcile};

type Key<'a> = (&'a Path, &'a str, &'a str);

fn tag(task: &Task) -> String {
    format!("task-{}", task.id)
}

pub(crate) fn lane_busy(root: &Path, task: &Task) -> bool {
    cache::tagged_busy(root, &task.workspace, &task.toolchain, &tag(task))
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

/// Renew only after the caller has released the task registry lock. An unmanaged
/// human worktree is intentionally a no-op.
pub(crate) fn renew(root: &Path, task: &Task) {
    if let Err(error) = worktree::touch(root, Path::new(&task.workspace)) {
        eprintln!(
            "grove: task {} activity is durable, but its worktree lease was not renewed: {error:#}",
            task.id
        );
    }
}

pub(super) fn start((root, repo, id): Key<'_>, argv: &[String]) -> Result<usize> {
    let (index, task) = {
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
            supervisor_pid: Some(std::process::id()),
            supervisor_start: process_start(std::process::id()),
            started_at: now_secs(),
            ended_at: None,
            exit_code: None,
            state: CommandState::Starting,
        });
        task.last_activity = now_secs();
        write(root, &task)?;
        (index, task)
    };
    renew(root, &task);
    Ok(index)
}

fn running((root, repo, id): Key<'_>, index: usize, pid: u32, start: u64) -> Result<()> {
    let task = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        let record = task
            .commands
            .get_mut(index)
            .context("task command record disappeared")?;
        record.pid = Some(pid);
        record.process_start = Some(start);
        record.state = CommandState::Running;
        task.last_activity = now_secs();
        write(root, &task)?;
        task
    };
    renew(root, &task);
    Ok(())
}

fn complete(
    (root, repo, id): Key<'_>,
    index: usize,
    code: Option<i32>,
    state: CommandState,
) -> Result<()> {
    let task = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        let record = task
            .commands
            .get_mut(index)
            .context("task command record disappeared")?;
        record.state = state;
        record.exit_code = code.or(Some(1));
        record.ended_at = Some(now_secs());
        task.last_activity = now_secs();
        write(root, &task)?;
        task
    };
    renew(root, &task);
    Ok(())
}

pub(super) fn pulse((root, repo, id): Key<'_>, index: usize) -> Result<()> {
    let task = {
        let _lock = claim::registry_lock(root, repo)?;
        let mut task = load(root, repo, id)?;
        let live = task.commands.get(index).is_some_and(|command| {
            matches!(
                command.state,
                CommandState::Starting | CommandState::Running
            )
        });
        if !live {
            return Ok(());
        }
        task.last_activity = now_secs();
        write(root, &task)?;
        task
    };
    renew(root, &task);
    Ok(())
}

/// Exit code recorded and returned when the supervisor kills a command at its
/// deadline; the `timeout(1)` convention orchestrators already classify. Like
/// timeout(1), this collides with a child that exits 124 on its own — the
/// durable record disambiguates (`interrupted` vs `exited`).
pub const EXIT_TIMEOUT: i32 = 124;
/// Exit code when the supervisor forwards a termination signal: 128 plus the
/// signal it received (143 for SIGTERM, 130 for SIGINT).
pub const EXIT_TERMINATED: i32 = 143;

/// What a supervised command may do. The build capability preserves the
/// established contract: the task's seeded lane is reserved for the command's
/// lifetime and its environment routes cargo there. The edit capability
/// supervises lifetime, signals, and the deadline without reserving a lane or
/// builder slot — grove builds the command runs acquire lanes on demand, so a
/// long-lived agent session never holds admission it is not using.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExecCapability {
    Build,
    Edit,
}

pub fn exec(
    root: &Path,
    repo: &str,
    id: &str,
    argv: &[String],
    timeout_secs: Option<u64>,
    capability: ExecCapability,
) -> Result<i32> {
    // Catch termination before the child exists: a signal in the Starting
    // window must still leave a reconciled record, not a wedged task.
    install_signal_forwarding();
    let deadline =
        timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs.min(31_536_000)));
    let key = (root, repo, id);
    if capability == ExecCapability::Edit {
        let snapshot = load(root, repo, id)?;
        worktree::full(root, Path::new(&snapshot.workspace))?;
        let snapshot = load(root, repo, id)?;
        return supervise(key, &snapshot, argv, deadline, None);
    }
    cache::maintain(root, || {
        let snapshot = load(root, repo, id)?;
        worktree::full(root, Path::new(&snapshot.workspace))?;
        let snapshot = load(root, repo, id)?;
        let grove =
            Grove::with_root_for_command(root.to_path_buf(), Path::new(&snapshot.workspace), argv);
        let cancelled = || pending_exit(deadline).is_some();
        let Some(lane) = grove.seeded_tagged_lane_until(&tag(&snapshot), &cancelled)? else {
            return Ok(pending_exit(deadline).unwrap_or(EXIT_TIMEOUT));
        };
        if let Some(code) = pending_exit(deadline) {
            return Ok(code);
        }
        supervise(key, &snapshot, argv, deadline, Some(&lane))
    })
}

fn supervise(
    key: Key<'_>,
    snapshot: &Task,
    argv: &[String],
    deadline: Option<Instant>,
    lane: Option<&cache::Lane>,
) -> Result<i32> {
    let index = start(key, argv)?;
    let (program, args) = argv.split_first().context("task exec requires a command")?;
    let mut command = Command::new(program);
    command.args(args).current_dir(&snapshot.workspace);
    if let Some(lane) = lane {
        cache::apply_env(&mut command, lane);
    }
    // Serialize the supervised command's git writes against every other
    // worktree's: a shim first on PATH routes them through the shared lock, so
    // a fleet never loses a commit to `.git` lock contention. Best-effort, so
    // supervision proceeds even when the shim cannot be written.
    #[cfg(unix)]
    if let Some(shim) = crate::gitgate::install_shim(key.0) {
        let mut path = std::ffi::OsString::from(shim);
        if let Some(existing) = std::env::var_os("PATH") {
            path.push(":");
            path.push(existing);
        }
        command.env("PATH", path);
    }
    // Its own process group, so a deadline or forwarded signal terminates
    // the executor and everything it spawned, not just the direct child.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // The record exists but no child does yet: a termination received by
    // now ends the attempt with a reconciled record instead of a spawn.
    if let Some(signal) = forwarded_signal() {
        complete(key, index, Some(128 + signal), CommandState::Interrupted)?;
        return Ok(128 + signal);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            complete(key, index, None, CommandState::Interrupted)?;
            return Err(error).with_context(|| format!("spawning {program}"));
        }
    };
    // Probe outside the registry lock so process inspection never stalls other tasks.
    let mut probed = process_start(child.id());
    for _ in 0..20 {
        if probed.is_some() || child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        probed = process_start(child.id());
    }
    if let Some(start) = probed {
        running(key, index, child.id(), start)?;
    }
    let mut pulse_due = Instant::now() + Duration::from_secs(5);
    // Once set, the classification is fixed: (exit code, escalation time).
    let mut termination: Option<(i32, Instant)> = None;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting for {program}"))?
        {
            break status;
        }
        let now = Instant::now();
        match termination {
            None => {
                let trigger = if let Some(signal) = forwarded_signal() {
                    Some((128 + signal, Some(signal)))
                } else if deadline.is_some_and(|at| now >= at) {
                    Some((EXIT_TIMEOUT, None))
                } else {
                    None
                };
                if let Some((code, signal)) = trigger {
                    // Prefer a genuine exit that raced the trigger over a
                    // synthetic classification; kill/wait is inherently
                    // racy, this closes the observable part of the window.
                    if let Some(status) = child
                        .try_wait()
                        .with_context(|| format!("waiting for {program}"))?
                    {
                        break status;
                    }
                    terminate_graceful(&mut child, signal);
                    termination = Some((code, now + Duration::from_secs(5)));
                }
            }
            Some((_, escalate_at)) if now >= escalate_at => {
                terminate_kill(&mut child);
            }
            Some(_) => {}
        }
        std::thread::sleep(Duration::from_secs(1));
        if Instant::now() >= pulse_due {
            pulse(key, index)?;
            pulse_due += Duration::from_secs(5);
        }
    };
    if let Some((code, _)) = termination {
        // The supervisor ended the command; record the classification, not
        // the raw signal death.
        complete(key, index, Some(code), CommandState::Interrupted)?;
        return Ok(code);
    }
    let state = if status.code().is_some() {
        CommandState::Exited
    } else {
        CommandState::Interrupted
    };
    complete(key, index, status.code(), state)?;
    Ok(status.code().unwrap_or(1))
}

fn pending_exit(deadline: Option<Instant>) -> Option<i32> {
    forwarded_signal().map(|signal| 128 + signal).or_else(|| {
        deadline
            .is_some_and(|at| Instant::now() >= at)
            .then_some(EXIT_TIMEOUT)
    })
}

#[cfg(unix)]
static RECEIVED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn note_signal(signal: libc::c_int) {
    RECEIVED_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_forwarding() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            note_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGINT, note_signal as *const () as libc::sighandler_t);
    }
}

/// Which termination signal arrived, so the child receives the same one and
/// the exit code says which (130 for SIGINT, 143 for SIGTERM).
#[cfg(unix)]
fn forwarded_signal() -> Option<i32> {
    match RECEIVED_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

/// Signal the child's whole process group AND the direct child: a command
/// that re-groups itself (setsid) escapes killpg, and the supervisor must
/// never spin forever behind a child it failed to reach.
#[cfg(unix)]
fn terminate_graceful(child: &mut std::process::Child, signal: Option<i32>) {
    let signal = signal.unwrap_or(libc::SIGTERM);
    unsafe {
        libc::killpg(child.id() as libc::pid_t, signal);
        libc::kill(child.id() as libc::pid_t, signal);
    }
}

#[cfg(unix)]
fn terminate_kill(child: &mut std::process::Child) {
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn install_signal_forwarding() {}

#[cfg(not(unix))]
fn forwarded_signal() -> Option<i32> {
    None
}

#[cfg(not(unix))]
fn terminate_graceful(child: &mut std::process::Child, _signal: Option<i32>) {
    let _ = child.kill();
}

/// Without process groups only the direct child can be stopped; grove's
/// build governance stays self-governed on Windows for the same reason.
#[cfg(not(unix))]
fn terminate_kill(child: &mut std::process::Child) {
    let _ = child.kill();
}

#[cfg(test)]
#[path = "task_activity_tests.rs"]
mod task_activity_tests;
