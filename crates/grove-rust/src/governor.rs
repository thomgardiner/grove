//! Machine-wide GNU jobserver governance for Grove-routed builds.
//!
//! `best_effort` shares tokens when its Unix FIFO is available and otherwise lets Cargo
//! govern itself. Each top-level builder retains an implicit jobserver slot, so this mode
//! is not a hard cap. `strict` uses a separate Unix pool, reserves one implicit slot for
//! every admitted builder, and refuses execution if its FIFO or admission locks cannot be
//! enforced. Its CPU accounting requires each admitted command to start at most one
//! top-level jobserver client, as Grove's direct Cargo paths do. Arbitrary `grove exec`
//! process trees can violate that rule and are admission-controlled but not CPU-bounded.
//! Neither mode controls non-jobserver work, memory, I/O, network, or unrelated processes.

use crate::config::{Governor, GovernorMode};
#[cfg(unix)]
use anyhow::Context;
use std::path::Path;

#[cfg(unix)]
#[path = "governor_lock.rs"]
mod lock;
#[path = "governor_process.rs"]
mod process;
#[cfg(unix)]
use lock::setup as lock_setup;

#[derive(Clone, Copy)]
pub(crate) enum Admission<'a> {
    Wait,
    Try,
    Until(&'a dyn Fn() -> bool),
}

pub(crate) enum Join {
    Ready(Option<Pool>),
    #[cfg_attr(not(unix), allow(dead_code))]
    Busy,
}

/// A held membership in one cache root's machine-wide build token pool.
#[cfg(unix)]
pub struct Pool {
    // Release admission before membership so queued builders can advance without setup.
    _admission: Option<std::fs::File>,
    _membership: std::fs::File,
    // Cargo's MAKEFLAGS parser treats spaces as argument separators. Keep two
    // inherited descriptors instead of publishing the FIFO pathname so cache
    // roots such as `/Volumes/THE VAULT/...` remain valid jobservers.
    fifo_read: std::fs::File,
    _fifo: std::fs::File,
}

#[cfg(not(unix))]
pub struct Pool;

impl Pool {
    /// Join the established best-effort pool, preserving the public legacy behavior.
    pub fn join(root: &Path, slots: usize) -> Option<Self> {
        Self::configured(root, Governor::best_effort(slots))
            .ok()
            .flatten()
    }

    /// Join the configured pool. Best-effort failures return no pool; strict failures
    /// refuse the caller so a build never runs without the requested enforcement.
    pub(crate) fn configured(root: &Path, governor: Governor) -> anyhow::Result<Option<Self>> {
        match Self::join_with(root, governor, Admission::Wait)? {
            Join::Ready(pool) => Ok(pool),
            Join::Busy => unreachable!("blocking admission cannot return busy"),
        }
    }

    pub(crate) fn join_with(
        root: &Path,
        governor: Governor,
        admission: Admission<'_>,
    ) -> anyhow::Result<Join> {
        let Governor {
            mode,
            cpu_slots: slots,
            max_builders,
        } = governor;
        #[cfg(unix)]
        {
            match mode {
                GovernorMode::BestEffort => match join_best_effort(root, slots) {
                    Ok(pool) => Ok(Join::Ready(Some(pool))),
                    Err(error) => {
                        eprintln!(
                            "grove: best-effort build governor unavailable ({error}); \
                             builds run self-governed"
                        );
                        Ok(Join::Ready(None))
                    }
                },
                GovernorMode::Strict => join_strict(root, slots, max_builders, admission)
                    .map(|pool| pool.map_or(Join::Busy, |pool| Join::Ready(Some(pool))))
                    .map_err(|error| anyhow::anyhow!("strict build governor unavailable: {error}")),
                GovernorMode::Invalid => {
                    anyhow::bail!("invalid build governor configuration")
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (root, slots, max_builders, admission);
            match mode {
                GovernorMode::BestEffort => Ok(Join::Ready(None)),
                GovernorMode::Strict => {
                    anyhow::bail!("strict build governor is supported only on Unix platforms")
                }
                GovernorMode::Invalid => {
                    anyhow::bail!("invalid build governor configuration")
                }
            }
        }
    }

    /// `MAKEFLAGS` pointing Cargo at this held pool. Best effort has no flags when its
    /// platform or setup cannot provide a FIFO jobserver.
    pub fn flags(&self) -> Option<String> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            Some(format!(
                "-j --jobserver-auth={},{}",
                self.fifo_read.as_raw_fd(),
                self._fifo.as_raw_fd()
            ))
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

#[cfg(unix)]
fn join_best_effort(root: &Path, slots: usize) -> anyhow::Result<Pool> {
    use anyhow::Context;
    use fs2::FileExt;
    use std::io::Write;

    let locks = lock_dir(root)?;
    let init = std::fs::File::create(locks.join("jobserver.lock"))
        .context("opening jobserver setup lock")?;
    init.lock_exclusive().context("locking jobserver setup")?;
    let path = root.join("jobserver");
    let mut fifo = open_fifo(&path, false)?;
    let fifo_read = fifo.try_clone().context("duplicating jobserver FIFO")?;
    let membership = std::fs::File::create(locks.join("jobserver-members.lock"))
        .context("opening jobserver membership lock")?;
    if membership.try_lock_exclusive().is_ok() {
        drain_best_effort(&mut fifo);
        fifo.write_all(&vec![b'+'; slots.saturating_sub(1)])
            .context("filling jobserver pool")?;
        FileExt::unlock(&membership).context("downgrading jobserver membership lock")?;
    }
    membership
        .lock_shared()
        .context("joining jobserver membership")?;
    Ok(Pool {
        _admission: None,
        _membership: membership,
        fifo_read,
        _fifo: fifo,
    })
}

#[cfg(unix)]
#[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
struct StrictPolicy {
    schema_version: u32,
    cpu_slots: usize,
    max_builders: usize,
}

#[cfg(unix)]
fn join_strict(
    root: &Path,
    slots: usize,
    max_builders: usize,
    admission: Admission<'_>,
) -> anyhow::Result<Option<Pool>> {
    validate_strict(slots, max_builders)?;
    let requested = StrictPolicy {
        schema_version: 1,
        cpu_slots: slots,
        max_builders,
    };
    let Some((fifo, membership, locks)) = strict_membership(root, &requested, admission)? else {
        return Ok(None);
    };
    let Some(admission) = admit(&locks, max_builders, admission)? else {
        return Ok(None);
    };
    let fifo_read = fifo
        .try_clone()
        .context("duplicating strict jobserver FIFO")?;
    Ok(Some(Pool {
        _admission: Some(admission),
        _membership: membership,
        fifo_read,
        _fifo: fifo,
    }))
}

#[cfg(unix)]
fn strict_membership(
    root: &Path,
    requested: &StrictPolicy,
    admission: Admission<'_>,
) -> anyhow::Result<Option<(std::fs::File, std::fs::File, std::path::PathBuf)>> {
    use anyhow::Context;
    use fs2::FileExt;

    let locks = lock_dir(root)?;
    let init = std::fs::File::create(locks.join("jobserver-strict.lock"))
        .context("opening strict setup lock")?;
    if !lock_setup(&init, admission).context("locking strict setup")? {
        return Ok(None);
    }
    let path = root.join("jobserver-strict");
    let mut fifo = open_fifo(&path, true)?;
    let membership = std::fs::File::create(locks.join("jobserver-strict-members.lock"))
        .context("opening strict membership lock")?;
    match membership.try_lock_exclusive() {
        Ok(()) => initialize_strict(&locks, &mut fifo, &membership, requested)?,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            require_active_policy(&locks, requested)?;
        }
        Err(error) => return Err(error).context("probing strict membership"),
    }
    membership
        .lock_shared()
        .context("joining strict membership")?;
    drop(init);
    Ok(Some((fifo, membership, locks)))
}

#[cfg(unix)]
fn initialize_strict(
    locks: &Path,
    fifo: &mut std::fs::File,
    membership: &std::fs::File,
    policy: &StrictPolicy,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use fs2::FileExt;
    use std::io::Write;

    drain_strict(fifo).context("draining strict jobserver pool")?;
    fifo.write_all(&vec![b'+'; policy.cpu_slots - policy.max_builders])
        .context("filling strict jobserver pool")?;
    let bytes = serde_json::to_vec(policy).context("encoding strict governor policy")?;
    std::fs::write(locks.join("jobserver-strict-policy.json"), bytes)
        .context("writing strict governor policy")?;
    FileExt::unlock(membership).context("downgrading strict membership lock")
}

#[cfg(unix)]
fn require_active_policy(locks: &Path, requested: &StrictPolicy) -> anyhow::Result<()> {
    use anyhow::Context;

    let bytes = std::fs::read(locks.join("jobserver-strict-policy.json"))
        .context("reading active strict governor policy")?;
    let active: StrictPolicy =
        serde_json::from_slice(&bytes).context("decoding active strict governor policy")?;
    if &active != requested {
        anyhow::bail!(
            "active strict pool uses cpu_slots={} and max_builders={}; requested {} and {}",
            active.cpu_slots,
            active.max_builders,
            requested.cpu_slots,
            requested.max_builders
        );
    }
    Ok(())
}

#[cfg(unix)]
fn validate_strict(slots: usize, max_builders: usize) -> anyhow::Result<()> {
    if max_builders == 0 {
        anyhow::bail!("max_builders must be at least one in strict mode");
    }
    if max_builders > slots {
        anyhow::bail!("max_builders ({max_builders}) exceeds cpu_slots ({slots})");
    }
    Ok(())
}

#[cfg(unix)]
fn admit(
    locks: &Path,
    max_builders: usize,
    admission: Admission<'_>,
) -> anyhow::Result<Option<std::fs::File>> {
    loop {
        match admission {
            Admission::Until(cancelled) if cancelled() => return Ok(None),
            Admission::Try => return try_admit(locks, max_builders),
            Admission::Wait | Admission::Until(_) => {}
        }
        if let Some(admission) = try_admit(locks, max_builders)? {
            return Ok(Some(admission));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn try_admit(locks: &Path, max_builders: usize) -> anyhow::Result<Option<std::fs::File>> {
    use anyhow::Context;
    use fs2::FileExt;

    for slot in 0..max_builders {
        let file =
            std::fs::File::create(locks.join(format!("jobserver-strict-builder-{slot}.lock")))
                .context("opening strict builder admission lock")?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("locking strict builder admission"),
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn lock_dir(root: &Path) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;

    let locks = root.join("locks");
    std::fs::create_dir_all(&locks).context("creating governor lock directory")?;
    Ok(locks)
}

#[cfg(unix)]
fn open_fifo(path: &Path, require_fifo: bool) -> anyhow::Result<std::fs::File> {
    use anyhow::Context;
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::FileTypeExt;

    if !path.exists() {
        let status = std::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(path)
            .status()
            .context("running mkfifo")?;
        if !status.success() {
            anyhow::bail!("mkfifo failed for {}", path.display());
        }
    }
    let fifo = rustix::fs::open(
        path,
        OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("opening jobserver fifo")?;
    let fifo = std::fs::File::from(fifo);
    if require_fifo && !fifo.metadata()?.file_type().is_fifo() {
        anyhow::bail!("{} is not a FIFO", path.display());
    }
    Ok(fifo)
}

#[cfg(unix)]
fn drain_best_effort(fifo: &mut std::fs::File) {
    let _ = drain_strict(fifo);
}

#[cfg(unix)]
fn drain_strict(fifo: &mut std::fs::File) -> std::io::Result<usize> {
    use std::io::Read;

    drain_with(|buf| fifo.read(buf))
}

#[cfg(unix)]
fn drain_with(mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>) -> std::io::Result<usize> {
    let mut total = 0;
    let mut buf = [0u8; 64];
    loop {
        match read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(read) => total += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(total),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(test, unix))]
#[path = "governor_tests.rs"]
mod tests;
