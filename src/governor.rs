//! Machine-wide build governance: one shared GNU-make jobserver pool under the cache
//! root. Every lane build inherits it through `MAKEFLAGS`, so N concurrent agents share
//! one CPU budget instead of each running a full `-j` build. Grove only creates and
//! fills the pool; cargo and rustc are already jobserver clients.
//!
//! A fifo's buffered tokens vanish when its last descriptor closes, so each grove
//! process keeps the fifo open and holds a shared membership lock while its build runs.
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

/// `MAKEFLAGS` pointing lane builds at the shared token pool. `None` when the platform
/// has no fifo jobserver or the pool cannot be prepared; builds then govern themselves.
/// The first call joins the pool for the life of this process.
pub fn makeflags(root: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        static POOL: std::sync::OnceLock<Option<Pool>> = std::sync::OnceLock::new();
        POOL.get_or_init(|| match join(root, crate::config::cpu_slots()) {
            Ok(pool) => Some(pool),
            Err(error) => {
                eprintln!("grove: build governor unavailable ({error}); builds run self-governed");
                None
            }
        })
        .as_ref()
        .map(|pool| format!("-j --jobserver-auth=fifo:{}", pool.path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        None
    }
}

/// Held for the life of the process: the fifo descriptor keeps the kernel buffer (the
/// tokens) alive, and the shared membership lock marks this process as an active
/// builder so joiners know not to refill.
#[cfg(unix)]
struct Pool {
    path: std::path::PathBuf,
    _fifo: std::fs::File,
    _membership: std::fs::File,
}

#[cfg(unix)]
fn join(root: &Path, slots: usize) -> anyhow::Result<Pool> {
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
        _fifo: fifo,
        _membership: membership,
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
            let first = join(root.path(), 4).unwrap();
            assert_eq!(drain(&mut open_pool(&first.path)), 3);
            // A joiner while builders are live must not refill, even with a new size:
            // the drain above emptied the pool, and it must stay empty.
            let second = join(root.path(), 9).unwrap();
            assert_eq!(drain(&mut open_pool(&second.path)), 0);
        }
        // Every holder dropped: the next builder starts an idle pool fresh, at the
        // currently configured size.
        let healed = join(root.path(), 6).unwrap();
        assert_eq!(drain(&mut open_pool(&healed.path)), 5);
    }

    #[test]
    fn makeflags_names_the_fifo_jobserver() {
        let root = tempfile::tempdir().unwrap();
        let flags = makeflags(root.path()).unwrap();
        assert!(flags.contains("--jobserver-auth=fifo:"));
    }
}
