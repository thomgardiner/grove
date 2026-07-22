//! Per-workspace build-lane identity, ownership, activity, and command environment.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

#[cfg(test)]
use super::write_atomic;
use super::{now_secs, short_hash};
use crate::config::{Config, Governor};
use crate::governor::Pool;

#[path = "cache_lane_acquire.rs"]
mod acquire;
pub use acquire::{SUPERVISED_LANE_ENV, acquire, acquire_tagged, try_acquire};
pub(crate) use acquire::{
    acquire_bootstrap_with_policy, acquire_bootstrap_with_policy_until, acquire_tagged_with_policy,
    acquire_tagged_with_policy_until, acquire_with_policy,
};

static UNREADABLE_POLICY_NONCE: OnceLock<String> = OnceLock::new();
pub(super) const UNVERIFIED_BOOTSTRAP_TAG: &str = "bootstrap-unverified";

#[derive(Clone)]
pub(crate) struct Policy {
    pub(crate) keep_debuginfo: bool,
    pub(crate) require_cow: bool,
    pub(crate) governor: Governor,
    pub(crate) min_free_gb: Option<u64>,
    pub(crate) max_canonical_gb: Option<u64>,
}

impl Policy {
    pub(crate) fn resolve(config: &Config) -> Self {
        Self {
            keep_debuginfo: config.debuginfo(),
            require_cow: config.cow(),
            governor: config.governor_limits(),
            min_free_gb: config.reserve(),
            max_canonical_gb: config.budget(),
        }
    }
}

pub fn lane_id(workspace: &str, toolchain: &str) -> String {
    lane_id_tagged(workspace, toolchain, "")
}

/// Lane id with an optional tag. An empty tag keys the untagged lane; incremental
/// policy changes mint a fresh lane rather than mixing incompatible artifacts.
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

/// Cargo incremental settings and Grove debug policy distinguish lanes. This digest is
/// a cache key; `grove doctor` exposes readable provenance.
pub(crate) fn lane_policy(workspace: &str, policy: &Policy) -> String {
    let mut hash = Sha256::new();
    hash.update(b"grove.lane-policy.v1\0");
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
    crate::hex(&hash.finalize())
}

#[derive(Serialize, Deserialize)]
pub(super) struct LaneMeta {
    pub(super) workspace: String,
    pub(super) toolchain: String,
    #[serde(default)]
    pub(super) tag: Option<String>,
    #[serde(default)]
    pub(super) policy_sha256: String,
    pub(super) last_used: u64,
}

pub struct Lane {
    pub dir: PathBuf,
    pub build_dir: PathBuf,
    pub target_dir: PathBuf,
    pub policy_sha256: String,
    pub(crate) keep_debuginfo: bool,
    pub(crate) require_cow: bool,
    pub(crate) governor: Governor,
    pool: Option<Pool>,
    _lock: File,
    _lifecycle: super::lifecycle::Guard,
}

pub fn apply_env(cmd: &mut Command, lane: &Lane) {
    cmd.env("CARGO_TARGET_DIR", &lane.target_dir);
    cmd.env("CARGO_BUILD_BUILD_DIR", &lane.build_dir);
    cmd.env(SUPERVISED_LANE_ENV, &lane.dir);
    apply_governor(cmd, lane);
    if !lane.keep_debuginfo {
        cmd.env("CARGO_PROFILE_DEV_DEBUG", "0");
        cmd.env("CARGO_PROFILE_TEST_DEBUG", "0");
        if cfg!(target_os = "macos") {
            cmd.env("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "off");
            cmd.env("CARGO_PROFILE_TEST_SPLIT_DEBUGINFO", "off");
        }
    }
}

pub(crate) fn apply_governor(cmd: &mut Command, lane: &Lane) {
    let Some(pool) = &lane.pool else {
        return;
    };
    pool.configure(cmd);
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        pool.inherit(cmd, lane._lock.as_raw_fd(), lane._lifecycle.raw_fd());
    }
}

fn lock_path(root: &Path, id: &str) -> PathBuf {
    root.join("locks").join(format!("{id}.lock"))
}

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

pub(super) fn lane_meta(dir: &Path) -> Option<LaneMeta> {
    serde_json::from_slice(&fs::read(dir.join(".grove-meta.json")).ok()?).ok()
}

pub fn lane_last_used(root: &Path, workspace: &str, toolchain: &str) -> Option<u64> {
    lane_meta(&root.join("lanes").join(lane_id(workspace, toolchain))).map(|m| m.last_used)
}

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
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'))
        })
        .collect()
}

pub(super) fn try_own(root: &Path, id: &str) -> Option<File> {
    let file = File::create(lock_path(root, id)).ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

pub fn discard(lane: Lane) {
    super::remove_lane_dir(&lane.dir);
}

/// Whether this lane is the workspace's persistent unverified bootstrap fallback.
/// Callers that reclaim tagged lanes must keep the bootstrap lane: it is the only
/// warm state a workspace has until a canonical it can seed from is published.
pub fn is_bootstrap(lane: &Lane) -> bool {
    lane_meta(&lane.dir).is_some_and(|meta| meta.tag.as_deref() == Some(UNVERIFIED_BOOTSTRAP_TAG))
}

#[cfg(test)]
#[path = "cache_lane_tests.rs"]
mod tests;
