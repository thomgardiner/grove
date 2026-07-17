//! Machine-wide build governance: one shared GNU-make jobserver pool under the cache
//! root. Every lane build inherits it through `MAKEFLAGS`, so N concurrent agents share
//! one CPU budget instead of each running a full `-j` build. Grove only creates and
//! fills the pool; cargo and rustc are already jobserver clients.
//!
//! A fifo's buffered tokens vanish when its last descriptor closes, so each live lane
//! keeps the fifo open and holds a shared membership lock while its build runs.
//! The first builder after an idle period takes the membership lock exclusively, drains
//! stale bytes, and refills `cpu_slots - 1` tokens. Tokens leaked by killed builds and
//! `cpu_slots` config changes therefore heal on every idle-to-active transition.
//!
//! Protocol note: each running build also holds one implicit token, so peak jobs is
//! roughly `cpu_slots + active builders - 1`. That bounds thrash by the number of
//! builders, not builders times cores.
//!
//! Failure never blocks a build: any error here degrades to self-governed builds.

use std::path::Path;

/// A held membership in one cache root's machine-wide build token pool. Dropping the
/// last live membership makes the next joiner authoritative to resize and repair it.
#[cfg(unix)]
pub struct Pool {
    path: std::path::PathBuf,
    // Drop membership first: an idle joiner may refill while this FIFO still keeps the
    // old kernel buffer alive. Closing the FIFO first can expose an empty live pool.
    _membership: std::fs::File,
    _fifo: std::fs::File,
}

#[cfg(not(unix))]
pub struct Pool;

impl Pool {
    /// Join `root`'s pool at its existing live size, or initialize an idle pool with
    /// `slots`. Failure degrades to a self-governed build.
    pub fn join(root: &Path, slots: usize) -> Option<Self> {
        #[cfg(unix)]
        {
            match join_fifo(root, slots) {
                Ok(pool) => Some(pool),
                Err(error) => {
                    eprintln!(
                        "grove: build governor unavailable ({error}); builds run self-governed"
                    );
                    None
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (root, slots);
            None
        }
    }

    /// `MAKEFLAGS` pointing Cargo at this held pool. `None` on platforms without a FIFO
    /// jobserver; builds then govern themselves.
    pub fn flags(&self) -> Option<String> {
        #[cfg(unix)]
        {
            Some(format!("-j --jobserver-auth=fifo:{}", self.path.display()))
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

#[cfg(unix)]
fn join_fifo(root: &Path, slots: usize) -> anyhow::Result<Pool> {
    use anyhow::Context;
    use fs2::FileExt;
    use rustix::fs::{Mode, OFlags};
    use std::io::Write;

    let locks = root.join("locks");
    std::fs::create_dir_all(&locks).context("creating lock directory")?;
    // Serializes join decisions; released when this function returns.
    let init =
        std::fs::File::create(locks.join("jobserver.lock")).context("opening jobserver lock")?;
    init.lock_exclusive().context("locking jobserver setup")?;

    let path = root.join("jobserver");
    if !path.exists() {
        // POSIX mkfifo: rustix does not expose mknodat on Apple targets.
        let status = std::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&path)
            .status()
            .context("running mkfifo")?;
        if !status.success() {
            anyhow::bail!("mkfifo failed for {}", path.display());
        }
    }
    // RDWR so the open never blocks awaiting a reader, as fifo semantics demand.
    let fifo = rustix::fs::open(
        &path,
        OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("opening jobserver fifo")?;
    let mut fifo = std::fs::File::from(fifo);

    let membership = std::fs::File::create(locks.join("jobserver-members.lock"))
        .context("opening membership lock")?;
    if membership.try_lock_exclusive().is_ok() {
        // No other builder is live: stale tokens are unaccounted, so start fresh.
        drain(&mut fifo);
        fifo.write_all(&vec![b'+'; slots.saturating_sub(1)])
            .context("filling jobserver pool")?;
        FileExt::unlock(&membership).context("downgrading membership lock")?;
    }
    membership
        .lock_shared()
        .context("joining jobserver membership")?;
    Ok(Pool {
        path,
        _membership: membership,
        _fifo: fifo,
    })
}

/// Read and discard every buffered token; returns how many were discarded.
#[cfg(unix)]
fn drain(fifo: &mut std::fs::File) -> usize {
    use std::io::Read;

    let mut total = 0;
    let mut buf = [0u8; 64];
    loop {
        match fifo.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    total
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rustix::fs::{Mode, OFlags};

    fn open_pool(path: &Path) -> std::fs::File {
        std::fs::File::from(
            rustix::fs::open(path, OFlags::RDWR | OFlags::NONBLOCK, Mode::empty()).unwrap(),
        )
    }

    #[test]
    fn first_builder_fills_and_the_pool_heals_on_idle() {
        let root = tempfile::tempdir().unwrap();
        {
            let first = join_fifo(root.path(), 4).unwrap();
            assert_eq!(drain(&mut open_pool(&first.path)), 3);
            // A joiner while builders are live must not refill, even with a new size:
            // the drain above emptied the pool, and it must stay empty.
            let second = join_fifo(root.path(), 9).unwrap();
            assert_eq!(drain(&mut open_pool(&second.path)), 0);
        }
        // Every holder dropped: the next builder starts an idle pool fresh, at the
        // currently configured size.
        let healed = join_fifo(root.path(), 6).unwrap();
        assert_eq!(drain(&mut open_pool(&healed.path)), 5);
    }

    #[test]
    fn flags_name_the_held_fifo_jobserver() {
        let root = tempfile::tempdir().unwrap();
        let flags = Pool::join(root.path(), 4).unwrap().flags().unwrap();
        assert!(flags.contains("--jobserver-auth=fifo:"));
    }
}
