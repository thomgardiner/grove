//! Per-workspace build-lane identity, ownership, activity, and command environment.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::{lifecycle_shared, now_secs, short_hash, write_atomic};
use crate::config::Config;
use crate::governor::Pool;

static UNREADABLE_POLICY_NONCE: OnceLock<String> = OnceLock::new();
const UNVERIFIED_BOOTSTRAP_TAG: &str = "bootstrap-unverified";

#[derive(Clone)]
pub(crate) struct Policy {
    pub(crate) keep_debuginfo: bool,
    pub(crate) require_cow: bool,
    pub(crate) cpu_slots: usize,
    pub(crate) min_free_gb: Option<u64>,
    pub(crate) max_canonical_gb: Option<u64>,
}

impl Policy {
    pub(crate) fn resolve(config: &Config) -> Self {
        Self {
            keep_debuginfo: config.debuginfo(),
            require_cow: config.cow(),
            cpu_slots: config.slots(),
            min_free_gb: config.reserve(),
            max_canonical_gb: config.budget(),
        }
    }
}

pub fn lane_id(workspace: &str, toolchain: &str) -> String {
    lane_id_tagged(workspace, toolchain, "")
}

/// Lane id with an optional tag, so one workspace can hold several independent lanes
/// (e.g. a long-running `verify` lane that must not block interactive `check`). An empty
/// tag keys the same lane as the untagged form. The incremental policy belongs in the
/// key: flipping it must create a fresh lane rather than mixing incompatible artifacts.
fn lane_id_tagged(workspace: &str, toolchain: &str, tag: &str) -> String {
    let config = Config::resolve(Path::new(workspace));
    let policy = Policy::resolve(&config);
    lane_id_with_policy(workspace, toolchain, tag, &lane_policy(workspace, &policy))
}

fn lane_id_with_policy(workspace: &str, toolchain: &str, tag: &str, policy: &str) -> String {
    if tag.is_empty() {
        short_hash(&[workspace, toolchain, policy])
    } else {
        short_hash(&[workspace, toolchain, policy, tag])
    }
}

/// Cargo's profile-level incremental setting, relevant override variables, and the
/// Grove debug policy distinguish lanes. The digest is deliberately a cache key rather
/// than a configuration report; `grove doctor` exposes the readable provenance.
pub(crate) fn lane_policy(workspace: &str, policy: &Policy) -> String {
    let mut hash = Sha256::new();
    hash.update(b"grove.lane-policy.v1\0");
    // A repository without Cargo has no build profile to distinguish lanes:
    // a stable token keeps non-Rust repositories quiet. Lanes still exist
    // there because their locks are the liveness signal reap and release
    // depend on. The noisy nonce is reserved for the genuine problem — a
    // Cargo workspace whose policy cannot be read.
    let incremental = if !crate::project::is_cargo_workspace(Path::new(workspace)) {
        "no-cargo-workspace".to_string()
    } else {
        crate::doctor::incremental_identity(Path::new(workspace)).unwrap_or_else(|error| {
            UNREADABLE_POLICY_NONCE
                .get_or_init(|| {
                    eprintln!(
                        "grove: cannot read the incremental build policy for {workspace}: \
                         {error}; builds in this process use a private cold lane; run \
                         grove doctor to diagnose"
                    );
                    format!("{}-{}", std::process::id(), now_secs())
                })
                .clone()
        })
    };
    hash.update(incremental.as_bytes());
    hash.update([u8::from(policy.keep_debuginfo)]);
    format!("{:x}", hash.finalize())
}

#[derive(Serialize, Deserialize)]
pub(super) struct LaneMeta {
    pub(super) workspace: String,
    toolchain: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    pub(super) policy_sha256: String,
    pub(super) last_used: u64,
}

/// A held build lane. The exclusive lock lives for the lane's lifetime, so a
/// concurrent grove (or GC) never touches a lane that is in use.
pub struct Lane {
    pub dir: PathBuf,
    pub build_dir: PathBuf,
    pub target_dir: PathBuf,
    pub policy_sha256: String,
    pub(crate) keep_debuginfo: bool,
    pub(crate) require_cow: bool,
    pub(crate) cpu_slots: usize,
    pool: Option<Pool>,
    _lock: File,
    _lifecycle: super::lifecycle::Guard,
}

/// Apply Grove's isolated build directories, shared build governor, and lean debug
/// profile to a command.
pub fn apply_env(cmd: &mut Command, lane: &Lane) {
    cmd.env("CARGO_TARGET_DIR", &lane.target_dir);
    cmd.env("CARGO_BUILD_BUILD_DIR", &lane.build_dir);
    if let Some(flags) = lane.pool.as_ref().and_then(Pool::flags) {
        cmd.env("MAKEFLAGS", flags);
    }
    if !lane.keep_debuginfo {
        cmd.env("CARGO_PROFILE_DEV_DEBUG", "0");
        cmd.env("CARGO_PROFILE_TEST_DEBUG", "0");
        if cfg!(target_os = "macos") {
            cmd.env("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "off");
            cmd.env("CARGO_PROFILE_TEST_SPLIT_DEBUGINFO", "off");
        }
    }
}

fn lock_path(root: &Path, id: &str) -> PathBuf {
    root.join("locks").join(format!("{id}.lock"))
}

/// Whether an existing tagged lane is locked by another process. This is a probe:
/// it never waits, creates a lane, or updates its activity metadata.
pub fn tagged_busy(root: &Path, workspace: &str, toolchain: &str, tag: &str) -> bool {
    lanes(root).into_iter().any(|dir| {
        let Some(meta) = lane_meta(&dir) else {
            return false;
        };
        if meta.workspace != workspace
            || meta.toolchain != toolchain
            || meta.tag.as_deref().is_some_and(|saved| saved != tag)
        {
            return false;
        }
        dir.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|id| lock_busy(root, id))
    }) || lock_busy(root, &lane_id_tagged(workspace, toolchain, tag))
}

fn lock_busy(root: &Path, id: &str) -> bool {
    let path = lock_path(root, id);
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    file.try_lock_exclusive().is_err()
}

/// Whether any known lane for `workspace` is currently held. Reap and recovery use this
/// broader probe before removing a leased worktree: tagged verify/exec/export lanes are
/// independent from the ordinary build lane, but must receive the same no-reap
/// protection. `exclude` names a lane id the caller itself holds (reap holds the
/// untagged lane while probing), which would otherwise always read as busy.
pub fn workspace_busy(root: &Path, workspace: &str, exclude: Option<&str>) -> bool {
    lanes(root).into_iter().any(|dir| {
        let Some(meta) = lane_meta(&dir) else {
            return false;
        };
        if meta.workspace != workspace {
            return false;
        }
        let Some(id) = dir.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if exclude == Some(id) {
            return false;
        }
        lock_busy(root, id)
    })
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
    let lock = File::create(lock_path(root, &id)).context("opening lane lock")?;
    Ok((dir, lock, policy_sha256))
}

/// Acquire the one persistent, explicitly unverified fallback lane shared by every
/// worktree of a repo while no verified canonical exists.
pub(crate) fn acquire_bootstrap_with_policy(
    root: &Path,
    workspace: &str,
    repo: &str,
    toolchain: &str,
    policy: &Policy,
) -> Result<Lane> {
    let lifecycle = lifecycle_shared(root, Path::new(workspace))?;
    let policy_sha256 = lane_policy(workspace, policy);
    let id = short_hash(&[repo, toolchain, &policy_sha256, UNVERIFIED_BOOTSTRAP_TAG]);
    let opened = open_lane_id(root, id, policy_sha256)?;
    opened
        .1
        .lock_exclusive()
        .context("locking bootstrap lane")?;
    finish_lane(
        root,
        opened,
        lifecycle,
        LaneSpec {
            workspace: repo,
            toolchain,
            tag: UNVERIFIED_BOOTSTRAP_TAG,
            policy,
        },
    )
}

fn finish_lane(
    root: &Path,
    opened: (PathBuf, File, String),
    lifecycle: super::lifecycle::Guard,
    spec: LaneSpec<'_>,
) -> Result<Lane> {
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
        cpu_slots: spec.policy.cpu_slots,
        pool: None,
        dir,
        _lock: lock,
        _lifecycle: lifecycle,
    };
    lane.pool = Pool::join(root, lane.cpu_slots);
    Ok(lane)
}

struct LaneSpec<'a> {
    workspace: &'a str,
    toolchain: &'a str,
    tag: &'a str,
    policy: &'a Policy,
}

/// Acquire the lane for `(workspace, toolchain)`, blocking until its exclusive lock
/// is free.
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

/// Acquire a tagged lane, so a caller can hold an independent lane (e.g. `verify`) that
/// does not contend with the interactive build lane. The lease/GC key on the real
/// workspace, so a tagged lane is still reclaimed when its worktree is gone.
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
    let lifecycle = lifecycle_shared(root, Path::new(workspace))?;
    let opened = open_lane(root, workspace, toolchain, tag, policy)?;
    opened.1.lock_exclusive().context("locking lane")?;
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
    )
}

/// Acquire the lane only if it is not already in use; `None` if another process holds
/// it. Used by prewarm so it never blocks or disturbs an agent's live build.
pub fn try_acquire(root: &Path, workspace: &str, toolchain: &str) -> Result<Option<Lane>> {
    let config = Config::resolve(Path::new(workspace));
    let policy = Policy::resolve(&config);
    let lifecycle = lifecycle_shared(root, Path::new(workspace))?;
    let opened = open_lane(root, workspace, toolchain, "", &policy)?;
    if opened.1.try_lock_exclusive().is_err() {
        return Ok(None);
    }
    Ok(Some(finish_lane(
        root,
        opened,
        lifecycle,
        LaneSpec {
            workspace,
            toolchain,
            tag: "",
            policy: &policy,
        },
    )?))
}

pub(super) fn lane_meta(dir: &Path) -> Option<LaneMeta> {
    serde_json::from_slice(&fs::read(dir.join(".grove-meta.json")).ok()?).ok()
}

/// When the lane for `(workspace, toolchain)` was last built in, if it exists. Every
/// `acquire` refreshes it, so the worktree pool reads it as an activity heartbeat to
/// decide when a worktree has gone idle long enough to be abandoned.
pub fn lane_last_used(root: &Path, workspace: &str, toolchain: &str) -> Option<u64> {
    lane_meta(&root.join("lanes").join(lane_id(workspace, toolchain))).map(|m| m.last_used)
}

/// The most recent `last_used` across ALL of a workspace's lanes — untagged and tagged
/// alike. The worktree pool reads this as the activity heartbeat: an agent working
/// through tagged `task exec`/`verify` lanes never touches the untagged lane, and an
/// untagged-only heartbeat would count it idle while it is hard at work.
pub fn workspace_last_used(root: &Path, workspace: &str) -> Option<u64> {
    lanes(root)
        .into_iter()
        .filter_map(|dir| lane_meta(&dir))
        .filter(|meta| meta.workspace == workspace)
        .map(|meta| meta.last_used)
        .max()
}

pub(super) fn lanes(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root.join("lanes"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        // Skip the dot-prefixed staging/backup dirs a clone leaves mid-swap; lane ids
        // are hex hashes, never dot-prefixed, so this only excludes transient scratch.
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'))
        })
        .collect()
}

/// Try to take the lane's lock; `None` if another process holds it (in use).
pub(super) fn try_own(root: &Path, id: &str) -> Option<File> {
    let file = File::create(lock_path(root, id)).ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

/// Reclaim a lane the caller still holds, the moment its work is done. A single-use
/// scratch lane (a tagged `exec`) is garbage once its command returns: keeping it only
/// hoards disk until the watermark forces eviction. Discarding now keeps just the
/// canonical warm; a re-run re-seeds from it copy-on-write. The lane's lock is held
/// across the delete (as `remove_lane_dir` requires) and released as `lane` drops.
pub fn discard(lane: Lane) {
    super::remove_lane_dir(&lane.dir);
}

#[cfg(test)]
#[path = "cache_lane_tests.rs"]
mod tests;
