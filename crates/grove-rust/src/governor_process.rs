use super::Pool;
use std::process::Command;

impl Pool {
    pub(crate) fn configure(&self, command: &mut Command) {
        if let Some(flags) = self.flags() {
            command.env("CARGO_MAKEFLAGS", &flags);
            command.env("MAKEFLAGS", &flags);
            command.env("MFLAGS", flags);
        }
    }

    #[cfg(unix)]
    pub(crate) fn inherit(
        &self,
        command: &mut Command,
        lane: std::os::fd::RawFd,
        lifecycle: std::os::fd::RawFd,
    ) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let mut fds = [
            self.fifo_read.as_raw_fd(),
            self._fifo.as_raw_fd(),
            -1,
            -1,
            -1,
            -1,
        ];
        if let Some(admission) = &self._admission {
            fds[2] = self._membership.as_raw_fd();
            fds[3] = admission.as_raw_fd();
            fds[4] = lane;
            fds[5] = lifecycle;
        }
        // SAFETY: after fork this closure only calls async-signal-safe fcntl with a
        // fixed stack array. Every descriptor remains owned by the live Lane until spawn.
        unsafe {
            command.pre_exec(move || {
                for fd in fds {
                    if fd < 0 {
                        continue;
                    }
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}
