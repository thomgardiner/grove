use super::*;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

pub(super) fn run(
    capsule: &Path,
    runtime: &Path,
    argv: &[String],
    timeout_secs: Option<u64>,
    stdout: &File,
    stderr: &File,
    started: impl FnOnce(u32) -> Result<()>,
) -> Result<Outcome> {
    let (program, args) = argv
        .split_first()
        .context("inspection exec requires a command")?;
    let stdout_capture = stdout.try_clone()?;
    let stderr_capture = stderr.try_clone()?;
    let mut child = command(program.as_ref(), runtime)
        .args(args)
        .current_dir(capsule)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let group = child.id() as libc::pid_t;
    let captures = match Captures::start(&mut child, stdout_capture, stderr_capture) {
        Ok(captures) => captures,
        Err(error) => {
            signal(group, libc::SIGKILL);
            let _ = child.wait();
            return Err(error);
        }
    };
    if let Err(error) = started(child.id()) {
        signal(group, libc::SIGKILL);
        let _ = child.wait();
        let _ = captures.finish();
        return Err(error);
    }
    let deadline =
        timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs.min(31_536_000)));
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            timed_out = true;
            signal(group, libc::SIGTERM);
            let grace = Instant::now() + Duration::from_secs(2);
            while Instant::now() < grace {
                if child.try_wait()?.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            signal(group, libc::SIGKILL);
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // A successful direct child may have left background descendants. They are
    // not part of the inspection result and must be gone before redigesting.
    signal(group, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(50));
    signal(group, libc::SIGKILL);
    let tree_clean = wait_empty(group, Duration::from_secs(5));
    let (stdout_truncated, stderr_truncated) = captures.finish()?;
    Ok(Outcome {
        exit_code: if timed_out {
            EXIT_TIMEOUT
        } else {
            status.code().unwrap_or(1)
        },
        timed_out,
        tree_clean,
        stdout_truncated,
        stderr_truncated,
    })
}

fn signal(group: libc::pid_t, signal: libc::c_int) {
    unsafe {
        libc::killpg(group, signal);
    }
}

fn wait_empty(group: libc::pid_t, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let live = unsafe { libc::killpg(group, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !live {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
