use super::*;
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};

pub(super) fn run(
    capsule: &Path,
    runtime: &Path,
    argv: &[String],
    timeout_secs: Option<u64>,
    stdout: &File,
    stderr: &File,
    started: impl FnOnce(u32) -> Result<()>,
) -> Result<Outcome> {
    let job = Job::new()?;
    let executable = std::env::current_exe()?;
    let stdout_capture = stdout.try_clone()?;
    let stderr_capture = stderr.try_clone()?;
    let mut helper = command(executable.as_os_str(), runtime);
    helper
        .args(["inspect", "__worker", "--"])
        .args(argv)
        .current_dir(capsule)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = helper.spawn()?;
    let captures = match Captures::start(&mut child, stdout_capture, stderr_capture) {
        Ok(captures) => captures,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    if let Err(error) = job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = captures.finish();
        return Err(error);
    }
    if let Err(error) = started(child.id()) {
        let _ = job.terminate(1);
        let _ = child.wait();
        let _ = captures.finish();
        return Err(error);
    }
    if let Err(error) = child
        .stdin
        .take()
        .context("inspection worker start pipe is unavailable")?
        .write_all(&[1])
    {
        let _ = job.terminate(1);
        let _ = child.wait();
        let _ = captures.finish();
        return Err(error).context("authorizing inspection worker");
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
            job.terminate(EXIT_TIMEOUT as u32)?;
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // Also remove descendants left behind by a normally exited worker.
    job.terminate(if timed_out { EXIT_TIMEOUT as u32 } else { 1 })?;
    let tree_clean = job.wait_empty(Duration::from_secs(5))?;
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

struct Job(HANDLE);

impl Job {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("creating inspection Job Object");
        }
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .context("configuring inspection Job Object");
        }
        Ok(job)
    }

    fn assign(&self, child: &std::process::Child) -> Result<()> {
        let handle = child.as_raw_handle() as HANDLE;
        if unsafe { AssignProcessToJobObject(self.0, handle) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("assigning blocked inspection worker to Job Object");
        }
        Ok(())
    }

    fn terminate(&self, code: u32) -> Result<()> {
        if unsafe { TerminateJobObject(self.0, code) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("terminating inspection Job Object");
        }
        Ok(())
    }

    fn wait_empty(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    std::ptr::from_mut(&mut accounting).cast(),
                    std::mem::size_of_val(&accounting) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("querying inspection Job Object");
            }
            if accounting.ActiveProcesses == 0 {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
