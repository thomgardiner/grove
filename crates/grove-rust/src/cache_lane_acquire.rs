use super::{Lane, LaneMeta, Policy, UNVERIFIED_BOOTSTRAP_TAG, lane_id_with_policy, lane_policy};
use crate::config::Config;
use crate::governor::{Admission, Join, Pool};
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use super::super::{lifecycle_shared, now_secs, short_hash, write_atomic};

/// Set in every command a lane runs (`task exec`, `exec --tag`, verification),
/// so a nested grove build can refuse instead of waiting on locks or builder
/// slots its own supervisor holds until the task deadline.
///
/// This is deadlock avoidance, not a security control: a child that clears its
/// environment (`env -i`, `sudo` without `-E`, a wrapper that rebuilds the
/// environment) loses the marker and goes back to blocking on the lane its
/// parent holds. Supervising with `--capability edit` is the durable fix,
/// because then the parent holds no lane to block on.
pub const SUPERVISED_LANE_ENV: &str = "GROVE_SUPERVISED_LANE";

fn refuse_nested_build() -> Result<()> {
    if std::env::var_os(SUPERVISED_LANE_ENV).is_none() {
        return Ok(());
    }
    anyhow::bail!(
        "this process already runs inside a grove-managed build lane; run cargo \
         directly (the lane environment routes it), or supervise the agent with \
         `grove task exec --capability edit` so its builds acquire lanes on demand"
    )
}

fn open_lane(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    tag: &str,
    policy: &Policy,
) -> Result<(PathBuf, File, String)> {
    let policy_sha256 = lane_policy(workspace, policy);
    let id = lane_id_with_policy(workspace, toolchain, tag, &policy_sha256);
    open_lane_id(root, id, policy_sha256)
}

fn open_lane_id(root: &Path, id: String, policy_sha256: String) -> Result<(PathBuf, File, String)> {
    let dir = root.join("lanes").join(&id);
    fs::create_dir_all(root.join("locks"))?;
    fs::create_dir_all(&dir)?;
    let lock = File::create(super::lock_path(root, &id)).context("opening lane lock")?;
    Ok((dir, lock, policy_sha256))
}

/// Acquire the workspace-scoped unverified fallback used while no canonical exists.
pub(crate) fn acquire_bootstrap_with_policy(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    policy: &Policy,
) -> Result<Lane> {
    acquire_bootstrap_with_policy_admission(root, workspace, toolchain, policy, Admission::Wait)?
        .context("blocking bootstrap admission returned busy")
}

pub(crate) fn acquire_bootstrap_with_policy_until(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    policy: &Policy,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Lane>> {
    acquire_bootstrap_with_policy_admission(
        root,
        workspace,
        toolchain,
        policy,
        Admission::Until(cancelled),
    )
}

fn acquire_bootstrap_with_policy_admission(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    policy: &Policy,
    admission: Admission<'_>,
) -> Result<Option<Lane>> {
    refuse_nested_build()?;
    let Some(lifecycle) = lifecycle(root, workspace, admission)? else {
        return Ok(None);
    };
    let policy_sha256 = lane_policy(workspace, policy);
    let id = short_hash(&[
        workspace,
        toolchain,
        &policy_sha256,
        UNVERIFIED_BOOTSTRAP_TAG,
    ]);
    let opened = open_lane_id(root, id, policy_sha256)?;
    if !lock_lane(&opened.1, admission).context("locking bootstrap lane")? {
        return Ok(None);
    }
    finish_lane(
        root,
        opened,
        lifecycle,
        LaneSpec {
            workspace,
            toolchain,
            tag: UNVERIFIED_BOOTSTRAP_TAG,
            policy,
        },
        admission,
    )
}

fn finish_lane(
    root: &Path,
    opened: (PathBuf, File, String),
    lifecycle: super::super::lifecycle::Guard,
    spec: LaneSpec<'_>,
    admission: Admission<'_>,
) -> Result<Option<Lane>> {
    let (dir, lock, policy_sha256) = opened;
    let meta = LaneMeta {
        workspace: spec.workspace.to_string(),
        toolchain: spec.toolchain.to_string(),
        tag: Some(spec.tag.to_string()),
        policy_sha256: policy_sha256.clone(),
        last_used: now_secs(),
    };
    write_atomic(&dir.join(".grove-meta.json"), &serde_json::to_vec(&meta)?)?;
    let mut lane = Lane {
        build_dir: dir.join("build"),
        target_dir: dir.join("target"),
        policy_sha256,
        keep_debuginfo: spec.policy.keep_debuginfo,
        require_cow: spec.policy.require_cow,
        governor: spec.policy.governor,
        pool: None,
        dir,
        _lock: lock,
        _lifecycle: lifecycle,
    };
    match Pool::join_with(root, lane.governor, admission)? {
        Join::Ready(pool) => {
            lane.pool = pool;
            Ok(Some(lane))
        }
        Join::Busy => Ok(None),
    }
}

fn lock_lane(file: &File, admission: Admission<'_>) -> Result<bool> {
    match admission {
        Admission::Wait => {
            file.lock_exclusive()?;
            Ok(true)
        }
        Admission::Try => match file.try_lock_exclusive() {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error.into()),
        },
        Admission::Until(cancelled) => loop {
            if cancelled() {
                return Ok(false);
            }
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        },
    }
}

struct LaneSpec<'a> {
    workspace: &'a str,
    toolchain: &'a str,
    tag: &'a str,
    policy: &'a Policy,
}

/// Acquire the lane for `(workspace, toolchain)`, blocking until it is free.
pub fn acquire(root: &Path, workspace: &str, toolchain: &str) -> Result<Lane> {
    let config = Config::resolve(Path::new(workspace));
    acquire_with_policy(root, workspace, toolchain, &Policy::resolve(&config))
}

pub(crate) fn acquire_with_policy(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    policy: &Policy,
) -> Result<Lane> {
    acquire_tagged_with_policy(root, workspace, toolchain, "", policy)
}

/// Acquire an independent tagged lane while retaining workspace lifecycle protection.
pub fn acquire_tagged(root: &Path, workspace: &str, toolchain: &str, tag: &str) -> Result<Lane> {
    let config = Config::resolve(Path::new(workspace));
    acquire_tagged_with_policy(root, workspace, toolchain, tag, &Policy::resolve(&config))
}

pub(crate) fn acquire_tagged_with_policy(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    tag: &str,
    policy: &Policy,
) -> Result<Lane> {
    acquire_tagged_with_policy_admission(root, workspace, toolchain, tag, policy, Admission::Wait)?
        .context("blocking lane admission returned busy")
}

pub(crate) fn acquire_tagged_with_policy_until(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    tag: &str,
    policy: &Policy,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Lane>> {
    acquire_tagged_with_policy_admission(
        root,
        workspace,
        toolchain,
        tag,
        policy,
        Admission::Until(cancelled),
    )
}

fn acquire_tagged_with_policy_admission(
    root: &Path,
    workspace: &str,
    toolchain: &str,
    tag: &str,
    policy: &Policy,
    admission: Admission<'_>,
) -> Result<Option<Lane>> {
    refuse_nested_build()?;
    let Some(lifecycle) = lifecycle(root, workspace, admission)? else {
        return Ok(None);
    };
    let opened = open_lane(root, workspace, toolchain, tag, policy)?;
    if !lock_lane(&opened.1, admission).context("locking lane")? {
        return Ok(None);
    }
    finish_lane(
        root,
        opened,
        lifecycle,
        LaneSpec {
            workspace,
            toolchain,
            tag,
            policy,
        },
        admission,
    )
}

/// Acquire a lane only if free; prewarm uses this to avoid blocking live builds.
pub fn try_acquire(root: &Path, workspace: &str, toolchain: &str) -> Result<Option<Lane>> {
    let config = Config::resolve(Path::new(workspace));
    let policy = Policy::resolve(&config);
    let Some(lifecycle) = lifecycle(root, workspace, Admission::Try)? else {
        return Ok(None);
    };
    let opened = open_lane(root, workspace, toolchain, "", &policy)?;
    if !lock_lane(&opened.1, Admission::Try)? {
        return Ok(None);
    }
    finish_lane(
        root,
        opened,
        lifecycle,
        LaneSpec {
            workspace,
            toolchain,
            tag: "",
            policy: &policy,
        },
        Admission::Try,
    )
}

fn lifecycle(
    root: &Path,
    workspace: &str,
    admission: Admission<'_>,
) -> Result<Option<super::super::lifecycle::Guard>> {
    let workspace = Path::new(workspace);
    match admission {
        Admission::Wait => Ok(Some(lifecycle_shared(root, workspace)?)),
        Admission::Try => super::super::lifecycle::try_shared(root, workspace),
        Admission::Until(cancelled) => {
            super::super::lifecycle::shared_until(root, workspace, cancelled)
        }
    }
}
