//! The shared cache: per-(workspace, toolchain) build lanes, one warm canonical per
//! (repo, toolchain), and a self-bounding garbage collector.
//!
//! Disk is bounded by three cooperating layers, in order of impact:
//!  1. a free-disk watermark on the real volume (`statfs`), the only CoW-safe signal;
//!  2. stale-lane GC — a lane whose worktree is gone is pure garbage;
//!  3. whole-lane LRU eviction when still over the watermark.
//!
//! Logical file sizes are never summed to decide eviction: copy-on-write clones
//! share blocks, so a logical sum overcounts and lies about real usage.

use sha2::{Digest, Sha256};
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "cache_atomic.rs"]
mod atomic;
pub use atomic::write_atomic;

#[path = "cache_lane.rs"]
mod lane;
pub use lane::{
    Lane, acquire, acquire_tagged, apply_env, discard, lane_id, lane_last_used, tagged_busy,
    try_acquire, workspace_busy, workspace_last_used,
};
pub(crate) use lane::{Policy, acquire_tagged_with_policy, acquire_with_policy};
use lane::{lane_meta, lanes, try_own};

#[path = "cache_lifecycle.rs"]
mod lifecycle;
pub(crate) use lifecycle::exclusive as lifecycle_exclusive;
pub(crate) use lifecycle::shared as lifecycle_shared;
pub(crate) use lifecycle::try_exclusive as lifecycle_try_exclusive;

#[path = "cache_seed.rs"]
mod seed_cache;
#[cfg(test)]
use seed_cache::CanonicalMeta;
use seed_cache::{canonical_last_used, canonical_lock, canonical_meta_path};
pub use seed_cache::{promote, seed};

#[path = "cache_gc.rs"]
mod gc;
use gc::remove_lane_dir;
pub use gc::{
    GcReport, LaneStatus, Status, enforce_canonical_budget, enforce_watermark, gc, maintain,
    max_canonical_gb, min_free_floor, reclaim_stale,
};
#[cfg(test)]
use gc::{
    MAX_FREE_FLOOR, MIN_FREE_FLOOR, canonicals, default_watermark_floor, evict_coldest_canonical,
};
pub(crate) use gc::{gc_with_policy, maintain_with_policy};

#[cfg(test)]
fn lane_policy(workspace: &str) -> String {
    let config = crate::config::Config::resolve(Path::new(workspace));
    lane::lane_policy(workspace, &Policy::resolve(&config))
}

pub fn cache_root() -> PathBuf {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::config::Config::resolve(&workspace).root()
}

pub fn reserve(root: &Path, config: &crate::config::Config) -> u64 {
    gc::watermark_floor(root, &Policy::resolve(config))
}

/// Fast cache inventory. Free space is physical; logical lane sizes are omitted.
pub fn status(root: &Path) -> Status {
    inventory(root, false)
}

/// Diagnostic cache inventory including logical per-lane sizes.
pub fn status_with_sizes(root: &Path) -> Status {
    inventory(root, true)
}

fn inventory(root: &Path, details: bool) -> Status {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = crate::config::Config::resolve(&workspace);
    status_with_policy(root, &Policy::resolve(&config), details)
}

pub(crate) fn status_with_policy(root: &Path, policy: &Policy, details: bool) -> Status {
    gc::status_inner(root, details, policy)
}

/// Resolve symlinks in `path`, falling back to the input. A workspace path must be the
/// same however it was reached — a build's `cargo locate-project` and prewarm's
/// `git worktree list` have to agree, or they key (and seed) different lanes.
pub fn canonical_path(path: &Path) -> PathBuf {
    grove_core::canonical_path(path)
}

fn short_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(parts.join("\0").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..20]
        .to_string()
}

/// A short stable slug for a repo identity, used to namespace that repo's worktrees in
/// one central directory instead of scattering them across the dev folder.
pub fn repo_slug(repo: &str) -> String {
    short_hash(&[repo])[..12].to_string()
}

/// Canonical is keyed by (repo, toolchain), NOT the lockfile: a dep bump would
/// otherwise mint a fresh empty canonical and force a cold rebuild, whereas Cargo
/// rebuilds only the changed deps when seeding from a drifted canonical.
pub fn canonical_dir(root: &Path, repo: &str, toolchain: &str) -> PathBuf {
    root.join("canonical").join(short_hash(&[repo, toolchain]))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn unreadable_policy_keys_a_stable_private_lane_within_the_process() {
        let missing = "/nonexistent/grove-policy-test";
        assert_eq!(lane_policy(missing), lane_policy(missing));
    }

    fn stamp_canonical(root: &Path, canonical: &Path, last_used: u64) {
        fs::create_dir_all(canonical.join("target")).unwrap();
        fs::write(canonical.join("target/x.rlib"), b"artifact").unwrap();
        let bytes = serde_json::to_vec(&CanonicalMeta { last_used }).unwrap();
        write_atomic(&canonical_meta_path(root, canonical), &bytes).unwrap();
    }

    #[test]
    fn evict_coldest_canonical_removes_the_least_recently_used() {
        let root = tempdir().unwrap();
        let warm = canonical_dir(root.path(), "warm-repo", "stable");
        let cold = canonical_dir(root.path(), "cold-repo", "stable");
        stamp_canonical(root.path(), &warm, 2_000);
        stamp_canonical(root.path(), &cold, 1_000);

        let evicted = evict_coldest_canonical(root.path()).expect("one canonical is evictable");

        assert!(evicted.starts_with("canonical:"));
        assert!(!cold.exists(), "the coldest canonical is evicted first");
        assert!(warm.exists(), "the warmer canonical is kept");
        // Its last-used record is cleaned up with it.
        assert!(!canonical_meta_path(root.path(), &cold).exists());
    }

    #[test]
    fn canonical_budget_evicts_until_under_the_cap() {
        let root = tempdir().unwrap();
        stamp_canonical(
            root.path(),
            &canonical_dir(root.path(), "a", "stable"),
            1_000,
        );
        stamp_canonical(
            root.path(),
            &canonical_dir(root.path(), "b", "stable"),
            2_000,
        );

        // A zero-GiB cap makes every canonical over-budget, so both are evicted. The env
        // read happens before the assert so a panic never leaks the var to sibling tests.
        // SAFETY: nextest runs each test in its own process, so no thread races on the env.
        unsafe { std::env::set_var("GROVE_MAX_CANONICAL_GB", "0") };
        let evicted = enforce_canonical_budget(root.path());
        unsafe { std::env::remove_var("GROVE_MAX_CANONICAL_GB") };

        assert_eq!(
            evicted.len(),
            2,
            "both canonicals are evicted under a 0 GiB cap"
        );
        assert_eq!(canonicals(root.path()).len(), 0);
    }

    #[test]
    fn gc_reclaims_a_gone_worktrees_lane_and_reports_it() {
        let root = tempdir().unwrap();
        let gone = root
            .path()
            .join("deleted-worktree")
            .to_string_lossy()
            .into_owned();
        fs::create_dir(&gone).unwrap();
        let lane = acquire(root.path(), &gone, "stable").unwrap();
        drop(lane); // release the lock so GC can claim it
        fs::remove_dir(&gone).unwrap();

        let report = gc(root.path());

        assert_eq!(
            report.reclaimed.len(),
            1,
            "the gone worktree's lane is reclaimed"
        );
    }

    #[test]
    fn discard_reclaims_a_held_tag_lane_immediately() {
        let root = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let workspace = workspace.path().to_string_lossy().into_owned();
        // A live workspace with disk far above the watermark: neither reclaim_stale (the
        // worktree exists) nor the watermark would touch this lane. Only discard does.
        let lane = acquire_tagged(root.path(), &workspace, "stable", "verify").unwrap();
        let dir = lane.dir.clone();
        fs::create_dir_all(lane.target_dir.join("deps")).unwrap();
        fs::write(lane.target_dir.join("deps/x.rlib"), b"artifact").unwrap();
        assert!(dir.exists());

        discard(lane);

        assert!(
            !dir.exists(),
            "a finished tag lane is reclaimed at once, not left until the watermark"
        );
    }

    #[test]
    fn default_watermark_tracks_volume_size_within_bounds() {
        let root = tempdir().unwrap();
        let total = fs2::total_space(root.path()).unwrap();

        assert_eq!(
            default_watermark_floor(root.path()),
            (total / 20).clamp(MIN_FREE_FLOOR, MAX_FREE_FLOOR)
        );
    }

    #[test]
    fn status_only_scans_logical_sizes_when_requested() {
        let root = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let workspace = workspace.path().to_string_lossy().into_owned();
        let lane = acquire(root.path(), &workspace, "stable").unwrap();
        fs::create_dir_all(&lane.target_dir).unwrap();
        fs::write(lane.target_dir.join("artifact"), b"bytes").unwrap();
        drop(lane);

        let quick = status(root.path());
        let detailed = status_with_sizes(root.path());

        assert_eq!(quick.lanes[0].size_bytes, None);
        assert!(detailed.lanes[0].size_bytes.is_some_and(|size| size >= 5));
    }

    #[test]
    fn workspace_last_used_covers_tagged_lanes() {
        let root = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let workspace = workspace.path().to_string_lossy().into_owned();
        drop(acquire(root.path(), &workspace, "stable").unwrap());
        let untagged = lane_last_used(root.path(), &workspace, "stable").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        drop(acquire_tagged(root.path(), &workspace, "stable", "task-x").unwrap());

        let all = workspace_last_used(root.path(), &workspace).unwrap();

        assert!(
            all > untagged,
            "the tagged lane's fresher activity must win: {all} vs {untagged}"
        );
    }

    #[test]
    fn gc_preserves_unauthenticated_staging_names() {
        let root = tempdir().unwrap();
        let lanes = root.path().join("lanes");
        let dead = lanes.join(".grove-staging-4294967294-0");
        let live = lanes.join(format!(".grove-staging-{}-0", std::process::id()));
        fs::create_dir_all(dead.join("target")).unwrap();
        fs::write(dead.join("target/leak.rlib"), b"leaked bytes").unwrap();
        fs::create_dir_all(&live).unwrap();

        let report = gc(root.path());

        assert!(
            dead.exists(),
            "a filename and dead-looking PID prove nothing"
        );
        assert!(
            live.exists(),
            "a filename and live-looking PID prove nothing"
        );
        assert!(
            report
                .reclaimed
                .iter()
                .all(|entry| !entry.starts_with("staging:")),
            "unauthenticated scratch must not be reported as reclaimed"
        );
    }

    #[test]
    fn gc_keeps_unauthenticated_canonical_staging_out_of_the_budget() {
        let root = tempdir().unwrap();
        let canonical = root.path().join("canonical");
        let dead = canonical.join(".grove-staging-4294967294-0");
        let live = canonical.join(format!(".grove-staging-{}-0", std::process::id()));
        fs::create_dir_all(&dead).unwrap();
        fs::write(dead.join("leak.rlib"), b"dead").unwrap();
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("artifact.rlib"), b"live").unwrap();

        // SAFETY: nextest runs each test in its own process, so no thread races on the env.
        unsafe {
            std::env::set_var("GROVE_MIN_FREE_GB", "0");
            std::env::set_var("GROVE_MAX_CANONICAL_GB", "0");
        }
        let report = gc(root.path());
        unsafe {
            std::env::remove_var("GROVE_MIN_FREE_GB");
            std::env::remove_var("GROVE_MAX_CANONICAL_GB");
        }

        assert!(
            dead.exists(),
            "dead-looking scratch has no deletion authority"
        );
        assert!(
            live.exists(),
            "live-looking scratch has no deletion authority"
        );
        assert!(report.evicted.is_empty());
    }

    #[test]
    fn maintain_reclaims_a_lane_after_its_work_finishes() {
        let root = tempdir().unwrap();
        let gone = root
            .path()
            .join("gone-worktree")
            .to_string_lossy()
            .into_owned();
        let lane_dir = maintain(root.path(), || {
            fs::create_dir(&gone).unwrap();
            let lane = acquire(root.path(), &gone, "stable").unwrap();
            fs::remove_dir(&gone).unwrap();
            lane.dir.clone()
        });

        assert!(
            !lane_dir.exists(),
            "post-work maintenance reclaims the released stale lane"
        );
    }
}
