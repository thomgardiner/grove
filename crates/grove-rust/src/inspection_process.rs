//! Process-tree supervision for untrusted inspection commands.

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

pub const EXIT_TIMEOUT: i32 = 124;
const MAX_LOG_BYTES: usize = 1024 * 1024;

pub struct Outcome {
    pub exit_code: i32,
    pub timed_out: bool,
    pub tree_clean: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub fn run(
    capsule: &Path,
    runtime: &Path,
    argv: &[String],
    timeout_secs: Option<u64>,
    stdout: &File,
    stderr: &File,
    started: impl FnOnce(u32) -> Result<()>,
) -> Result<Outcome> {
    let (program, _) = argv
        .split_first()
        .context("inspection exec requires a command")?;
    fs::create_dir_all(runtime)?;
    fs::write(runtime.join("gitconfig"), b"")?;
    platform::run(
        capsule,
        runtime,
        argv,
        timeout_secs,
        stdout,
        stderr,
        started,
    )
    .with_context(|| format!("supervising inspection command {program}"))
}

/// Hidden child entrypoint. On Windows it blocks until its parent has assigned
/// it to the kill-on-close Job Object, closing the spawn-before-assignment race.
pub fn worker(argv: &[String]) -> Result<i32> {
    let mut start = [0u8; 1];
    std::io::stdin()
        .read_exact(&mut start)
        .context("inspection worker was not authorized by its supervisor")?;
    if start != [1] {
        bail!("inspection worker received an invalid start token")
    }
    let (program, args) = argv
        .split_first()
        .context("inspection worker requires a command")?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("spawning inspection command {program}"))?;
    Ok(status.code().unwrap_or(1))
}

fn command(program: &OsStr, runtime: &Path) -> Command {
    let mut command = Command::new(program);
    for (name, _) in std::env::vars_os() {
        if starts_with(&name, "GIT_") {
            command.env_remove(name);
        }
    }
    for name in ["SSH_AUTH_SOCK", "SSH_ASKPASS", "GITHUB_TOKEN", "GH_TOKEN"] {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_GLOBAL", runtime.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn starts_with(name: &OsStr, prefix: &str) -> bool {
    name.to_string_lossy()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

struct Captures {
    stdout: JoinHandle<Result<bool>>,
    stderr: JoinHandle<Result<bool>>,
    #[cfg(unix)]
    cancel: Arc<AtomicBool>,
}

impl Captures {
    fn start(child: &mut Child, stdout: File, stderr: File) -> Result<Self> {
        let child_stdout = child
            .stdout
            .take()
            .context("inspection stdout unavailable")?;
        let child_stderr = child
            .stderr
            .take()
            .context("inspection stderr unavailable")?;
        nonblocking(&child_stdout)?;
        nonblocking(&child_stderr)?;
        let cancel = Arc::new(AtomicBool::new(false));
        Ok(Self {
            stdout: capture(child_stdout, stdout, Arc::clone(&cancel)),
            stderr: capture(child_stderr, stderr, Arc::clone(&cancel)),
            #[cfg(unix)]
            cancel,
        })
    }

    fn finish(self) -> Result<(bool, bool)> {
        #[cfg(unix)]
        {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !(self.stdout.is_finished() && self.stderr.is_finished()) {
                if Instant::now() >= deadline {
                    self.cancel.store(true, Ordering::Release);
                    let _ = self.stdout.join();
                    let _ = self.stderr.join();
                    bail!("inspection output pipes remained open after process cleanup")
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let stdout = self
            .stdout
            .join()
            .map_err(|_| anyhow!("inspection stdout capture panicked"))??;
        let stderr = self
            .stderr
            .join()
            .map_err(|_| anyhow!("inspection stderr capture panicked"))??;
        Ok((stdout, stderr))
    }
}

fn capture(
    mut input: impl Read + Send + 'static,
    mut output: File,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<Result<bool>> {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        let mut written = 0usize;
        let mut truncated = false;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Ok(truncated);
            }
            let read = match input.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if read == 0 {
                return Ok(truncated);
            }
            let keep = read.min(MAX_LOG_BYTES.saturating_sub(written));
            output.write_all(&buffer[..keep])?;
            written += keep;
            truncated |= keep != read;
        }
    })
}

#[cfg(unix)]
fn nonblocking(pipe: &impl std::os::fd::AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("configuring inspection output pipe");
    }
    Ok(())
}

#[cfg(not(unix))]
fn nonblocking<T>(_pipe: &T) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
#[path = "inspection_process_unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "inspection_process_windows.rs"]
mod platform;

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    pub(super) fn run(
        _capsule: &Path,
        _runtime: &Path,
        _argv: &[String],
        _timeout_secs: Option<u64>,
        _stdout: &File,
        _stderr: &File,
        _started: impl FnOnce(u32) -> Result<()>,
    ) -> Result<Outcome> {
        bail!("inspection process containment is unavailable on this platform")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn capture_cancels_while_a_writer_keeps_the_pipe_open() {
        let (reader, writer) = UnixStream::pair().unwrap();
        nonblocking(&reader).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = capture(reader, tempfile::tempfile().unwrap(), Arc::clone(&cancel));
        let (release, safeguard_release) = std::sync::mpsc::channel();
        let safeguard = std::thread::spawn(move || {
            let _ = safeguard_release.recv_timeout(Duration::from_secs(3));
            drop(writer);
        });
        let started = Instant::now();
        cancel.store(true, Ordering::Release);
        assert!(!capture.join().unwrap().unwrap());
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = release.send(());
        safeguard.join().unwrap();
    }
}
