//! Cache reclamation, eviction policy, evidence GC orchestration, and inventory.

use fs2::FileExt;
use serde::Serialize;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use super::{
    Policy, canonical_last_used, canonical_lock, canonical_meta_path, lane_meta, lanes, try_own,
};

pub(super) const MIN_FREE_FLOOR: u64 = 20 * 1024 * 1024 * 1024;
pub(super) const MAX_FREE_FLOOR: u64 = 50 * 1024 * 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;
fn current() -> (PathBuf, Policy) {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = crate::config::Config::resolve(&workspace);
    (config.root(), Policy::resolve(&config))
}

fn tree_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Try to take the one cache-wide GC lock; `None` if another process already holds it.
/// Eviction is serialized through it so N agents building at once cannot each start
/// evicting and stampede past the watermark into a deletion storm — the first to arrive
/// reclaims, the rest skip and let it.
fn try_gc_lock(root: &Path) -> Option<File> {
    fs::create_dir_all(root.join("locks")).ok()?;
    let file = File::create(root.join("locks").join("gc.lock")).ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

/// Remove a lane's directory. The caller must hold the lane's lock across this call.
/// Returns whether the directory is gone afterward. The lock file is deliberately
/// left in place: unlinking it lets one process keep a lock on the old inode while
/// another locks a freshly created one for the same lane (split-brain ownership).
pub(super) fn remove_lane_dir(dir: &Path) -> bool {
    fs::remove_dir_all(dir).is_ok() || !dir.exists()
}

/// Reclaim lanes whose worktree no longer exists. Never touches an in-use lane.
pub fn reclaim_stale(root: &Path) -> Vec<String> {
    let mut reclaimed = Vec::new();
    for dir in lanes(root) {
        let id = dir.file_name().unwrap().to_string_lossy().into_owned();
        let Some(meta) = lane_meta(&dir) else {
            continue;
        };
        if Path::new(&meta.workspace).exists() {
            continue;
        }
        if let Some(_lock) = try_own(root, &id)
            && remove_lane_dir(&dir)
        {
            reclaimed.push(id);
        }
    }
    reclaimed
}

pub(super) fn canonicals(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root.join("canonical"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        // Clone staging and backups are reclaimed by `sweep_dead_staging`; a live one
        // must never become an ordinary canonical eviction candidate.
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'))
        })
        .collect()
}

/// How much free disk to keep. An explicit `GROVE_MIN_FREE_GB` wins; otherwise retain
/// 5% of the volume, bounded between 20 and 50 GiB. The cap avoids asking a large volume
/// for an impractical reserve, while the percentage gives a nearly full large disk room to
/// finish a build before eviction begins.
pub fn min_free_floor() -> u64 {
    let (root, policy) = current();
    watermark_floor(&root, &policy)
}

pub(super) fn default_watermark_floor(root: &Path) -> u64 {
    root.ancestors()
        .find_map(|path| fs2::total_space(path).ok())
        .map(|total| (total / 20).clamp(MIN_FREE_FLOOR, MAX_FREE_FLOOR))
        .unwrap_or(MIN_FREE_FLOOR)
}

pub(super) fn watermark_floor(root: &Path, policy: &Policy) -> u64 {
    policy
        .min_free_gb
        .map(|gb| gb.saturating_mul(GIB))
        .unwrap_or_else(|| default_watermark_floor(root))
}

/// Evict whole lanes, least-recently-used first, until real free disk clears the
/// watermark, then evict the coldest canonicals if lanes were not enough. Skips in-use
/// lanes and re-measures free disk after each removal (deleting a copy-on-write clone can
/// free nothing until the last sharer dies). Serialized through the GC lock: if another
/// process is already reclaiming, this returns empty rather than piling on.
pub fn enforce_watermark(root: &Path) -> Vec<String> {
    let floor = watermark_floor(root, &current().1);
    let Some(_gc) = try_gc_lock(root) else {
        return Vec::new();
    };
    enforce_watermark_locked(root, floor)
}

fn enforce_watermark_locked(root: &Path, floor: u64) -> Vec<String> {
    let mut evicted = Vec::new();
    loop {
        let free = fs2::available_space(root).unwrap_or(u64::MAX);
        if free >= floor {
            break;
        }
        let mut candidates: Vec<(u64, PathBuf, String)> = lanes(root)
            .into_iter()
            .filter_map(|dir| {
                let id = dir.file_name()?.to_string_lossy().into_owned();
                let last = lane_meta(&dir)?.last_used;
                Some((last, dir, id))
            })
            .collect();
        candidates.sort_by_key(|(last, _, _)| *last);
        // Evict the LRU lane we can both lock and delete, holding the lock across the
        // delete so a concurrent build can never land on a lane mid-eviction. A lane
        // we cannot lock (in use) or cannot delete is left alone, and a failed delete
        // is never counted as evicted — so enforcement can never loop on it forever.
        let mut removed_one = false;
        for (_, dir, id) in candidates {
            let Some(_lock) = try_own(root, &id) else {
                continue;
            };
            if remove_lane_dir(&dir) {
                evicted.push(id);
                removed_one = true;
                break;
            }
        }
        if !removed_one {
            break; // no lane evictable (all in use or undeletable)
        }
    }
    // Phase 2: lanes were not enough. Evict the coldest canonicals — the real disk (deps
    // and artifacts) — that no seed or promote is currently using.
    while fs2::available_space(root).unwrap_or(u64::MAX) < floor {
        match evict_coldest_canonical(root) {
            Some(name) => evicted.push(name),
            None => {
                eprintln!(
                    "grove: {} GiB free is below the floor and nothing more is evictable; \
                     builds may run cold or fail on a full disk",
                    fs2::available_space(root).unwrap_or(0) / (1024 * 1024 * 1024)
                );
                break;
            }
        }
    }
    evicted
}

/// Evict the single coldest canonical that no seed or promote currently holds, holding
/// its exclusive lock across the delete. Returns its `canonical:<id>` label, or `None` if
/// none is lockable and removable. Callers loop on their own bound (watermark or budget).
pub(super) fn evict_coldest_canonical(root: &Path) -> Option<String> {
    let mut cans: Vec<(u64, PathBuf)> = canonicals(root)
        .into_iter()
        .map(|d| (canonical_last_used(root, &d), d))
        .collect();
    cans.sort_by_key(|(last, _)| *last);
    for (_, dir) in cans {
        let Ok(lock) = canonical_lock(root, &dir) else {
            continue;
        };
        if lock.try_lock_exclusive().is_err() {
            continue; // a live seed or promote holds it
        }
        if remove_lane_dir(&dir) {
            let _ = fs::remove_file(canonical_meta_path(root, &dir));
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Some(format!("canonical:{name}"));
        }
    }
    None
}

/// The optional hard cap on total warm-build (canonical) size, in GiB. Unset means the
/// free-disk watermark is the only bound. `GROVE_MAX_CANONICAL_GB`, then config.
pub fn max_canonical_gb() -> Option<u64> {
    current().1.max_canonical_gb
}

fn canonical_total_size(root: &Path) -> u64 {
    canonicals(root).iter().map(|d| tree_size(d)).sum()
}

/// Evict the coldest canonicals until their combined size is within `max_canonical_gb`.
/// Independent of the free-disk watermark, so the warm-build cache stays bounded even on
/// a large disk. A no-op when no budget is configured. Serialized through the GC lock.
pub fn enforce_canonical_budget(root: &Path) -> Vec<String> {
    let budget = current()
        .1
        .max_canonical_gb
        .map(|gb| gb.saturating_mul(GIB));
    let Some(_gc) = try_gc_lock(root) else {
        return Vec::new();
    };
    enforce_canonical_budget_locked(root, budget)
}

fn enforce_canonical_budget_locked(root: &Path, budget: Option<u64>) -> Vec<String> {
    let Some(budget) = budget else {
        return Vec::new();
    };
    let mut evicted = Vec::new();
    while canonical_total_size(root) > budget {
        match evict_coldest_canonical(root) {
            Some(name) => evicted.push(name),
            None => break, // remaining canonicals are all locked/in use
        }
    }
    evicted
}

/// Remove clone staging/backup directories whose owning process died mid-swap. They are
/// dot-prefixed, so `lanes()` (correctly) hides them from lane GC and eviction — but
/// without this sweep a crashed clone leaks a whole target-dir copy that the watermark
/// can neither see nor evict. Cleanup requires the creator's durable owner sidecar.
fn sweep_dead_staging(root: &Path) -> Vec<String> {
    let mut swept = Vec::new();
    for base in [root.join("lanes"), root.join("canonical")] {
        for entry in fs::read_dir(&base).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with(".grove-staging-") && !name.starts_with(".grove-old-") {
                continue;
            }
            if crate::seed::reap(&path) {
                swept.push(format!("staging:{name}"));
            }
        }
    }
    swept
}

#[derive(Serialize, Default)]
pub struct GcReport {
    pub reclaimed: Vec<String>,
    pub evicted: Vec<String>,
    pub evidence_reclaimed: Vec<String>,
    pub floor_bytes: u64,
    pub canonical_budget_bytes: Option<u64>,
}

/// Full garbage collection under a single GC lock: reclaim lanes whose worktree is gone,
/// sweep dead clones' staging scratch and stale verification evidence, evict lanes to the
/// free-disk watermark, then evict canonicals to the configured budget.
pub fn gc(root: &Path) -> GcReport {
    gc_with_policy(root, &current().1)
}

pub(crate) fn gc_with_policy(root: &Path, policy: &Policy) -> GcReport {
    let floor_bytes = watermark_floor(root, policy);
    let canonical_budget_bytes = policy.max_canonical_gb.map(|gb| gb.saturating_mul(GIB));
    let mut reclaimed = reclaim_stale(root);
    let Some(_gc) = try_gc_lock(root) else {
        return GcReport {
            reclaimed,
            evicted: Vec::new(),
            evidence_reclaimed: Vec::new(),
            floor_bytes,
            canonical_budget_bytes,
        };
    };
    reclaimed.extend(sweep_dead_staging(root));
    let evidence_reclaimed = crate::verify::reclaim_evidence(root);
    let mut evicted = enforce_watermark_locked(root, floor_bytes);
    evicted.extend(enforce_canonical_budget_locked(
        root,
        canonical_budget_bytes,
    ));
    GcReport {
        reclaimed,
        evicted,
        evidence_reclaimed,
        floor_bytes,
        canonical_budget_bytes,
    }
}

/// Run cache maintenance before and after `work`. Lanes created inside the closure are
/// dropped before the second pass, so an over-full volume can evict the just-finished
/// lane if older cache entries were not enough.
pub fn maintain<T>(root: &Path, work: impl FnOnce() -> T) -> T {
    maintain_with_policy(root, &current().1, work)
}

pub(crate) fn maintain_with_policy<T>(root: &Path, policy: &Policy, work: impl FnOnce() -> T) -> T {
    gc_with_policy(root, policy);
    let result = work();
    gc_with_policy(root, policy);
    result
}

#[derive(Serialize)]
pub struct Status {
    pub root: String,
    pub free_bytes: u64,
    pub floor_bytes: u64,
    pub lane_count: usize,
    pub lanes: Vec<LaneStatus>,
}

#[derive(Serialize)]
pub struct LaneStatus {
    pub id: String,
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    pub last_used: u64,
}

pub(super) fn status_inner(root: &Path, include_sizes: bool, policy: &Policy) -> Status {
    let lanes: Vec<LaneStatus> = lanes(root)
        .into_iter()
        .map(|dir| {
            let id = dir.file_name().unwrap().to_string_lossy().into_owned();
            let meta = lane_meta(&dir);
            LaneStatus {
                id,
                workspace: meta.as_ref().map(|m| m.workspace.clone()),
                policy_sha256: meta
                    .as_ref()
                    .and_then(|m| (!m.policy_sha256.is_empty()).then(|| m.policy_sha256.clone())),
                size_bytes: include_sizes.then(|| tree_size(&dir)),
                last_used: meta.map(|m| m.last_used).unwrap_or(0),
            }
        })
        .collect();
    Status {
        root: root.display().to_string(),
        free_bytes: fs2::available_space(root).unwrap_or(0),
        floor_bytes: watermark_floor(root, policy),
        lane_count: lanes.len(),
        lanes,
    }
}
