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

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically: fsync a temp sibling, then rename it into place.
/// A crash leaves either the old file or the complete new one, never a half-written file
/// that a `serde_json::from_slice(...).ok()` reader would silently drop as if the record
/// never existed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("f");
    let tmp = parent.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = File::create(&tmp).context("creating temp file")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path).context("publishing temp file")?;
    Ok(())
}

/// A lock guarding one canonical against seed/promote races: seeds take it shared (many
/// lanes clone at once), a promote takes it exclusive (rewrites it alone), so no seed
/// ever reads a canonical mid-rewrite.
fn canonical_lock(root: &Path, canonical: &Path) -> Result<File> {
    let name = canonical
        .file_name()
        .context("canonical path has no name")?
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(root.join("locks"))?;
    File::create(root.join("locks").join(format!("canonical-{name}.lock")))
        .context("opening canonical lock")
}

const MIN_FREE_FLOOR: u64 = 20 * 1024 * 1024 * 1024; // 20 GiB absolute floor

pub fn cache_root() -> PathBuf {
    if let Ok(explicit) = std::env::var("GROVE_CACHE_ROOT") {
        return PathBuf::from(explicit);
    }
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cargo"));
    cargo_home.join("grove")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve symlinks in `path`, falling back to the input. A workspace path must be the
/// same however it was reached — a build's `cargo locate-project` and prewarm's
/// `git worktree list` have to agree, or they key (and seed) different lanes.
pub fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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

pub fn lane_id(workspace: &str, toolchain: &str) -> String {
    short_hash(&[workspace, toolchain])
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

#[derive(Serialize, Deserialize)]
struct LaneMeta {
    workspace: String,
    toolchain: String,
    last_used: u64,
}

/// A held build lane. The exclusive lock lives for the lane's lifetime, so a
/// concurrent grove (or GC) never touches a lane that is in use.
pub struct Lane {
    pub dir: PathBuf,
    pub build_dir: PathBuf,
    pub target_dir: PathBuf,
    _lock: File,
}

fn lock_path(root: &Path, id: &str) -> PathBuf {
    root.join("locks").join(format!("{id}.lock"))
}

fn open_lane(root: &Path, workspace: &str, toolchain: &str) -> Result<(PathBuf, File)> {
    let id = lane_id(workspace, toolchain);
    let dir = root.join("lanes").join(&id);
    fs::create_dir_all(root.join("locks"))?;
    fs::create_dir_all(&dir)?;
    let lock = File::create(lock_path(root, &id)).context("opening lane lock")?;
    Ok((dir, lock))
}

fn finish_lane(dir: PathBuf, lock: File, workspace: &str, toolchain: &str) -> Result<Lane> {
    let meta = LaneMeta {
        workspace: workspace.to_string(),
        toolchain: toolchain.to_string(),
        last_used: now_secs(),
    };
    write_atomic(&dir.join(".grove-meta.json"), &serde_json::to_vec(&meta)?)?;
    Ok(Lane {
        build_dir: dir.join("build"),
        target_dir: dir.join("target"),
        dir,
        _lock: lock,
    })
}

/// Acquire the lane for `(workspace, toolchain)`, blocking until its exclusive lock
/// is free.
pub fn acquire(root: &Path, workspace: &str, toolchain: &str) -> Result<Lane> {
    let (dir, lock) = open_lane(root, workspace, toolchain)?;
    lock.lock_exclusive().context("locking lane")?;
    finish_lane(dir, lock, workspace, toolchain)
}

/// Acquire the lane only if it is not already in use; `None` if another process holds
/// it. Used by prewarm so it never blocks or disturbs an agent's live build.
pub fn try_acquire(root: &Path, workspace: &str, toolchain: &str) -> Result<Option<Lane>> {
    let (dir, lock) = open_lane(root, workspace, toolchain)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(None);
    }
    Ok(Some(finish_lane(dir, lock, workspace, toolchain)?))
}

/// Seed a cold lane from its canonical (copy-on-write). A lane that already holds a
/// `target/` is warm and is left untouched. Holds the canonical's lock shared, so it
/// never clones a canonical a concurrent promote is rewriting.
pub fn seed(root: &Path, lane: &Lane, canonical: &Path) -> Result<bool> {
    if lane.target_dir.exists() || !canonical.exists() {
        return Ok(false);
    }
    let lock = canonical_lock(root, canonical)?;
    lock.lock_shared()
        .context("shared-locking canonical for seed")?;
    if !canonical.exists() {
        return Ok(false); // a promote removed it between the check and the lock
    }
    // Clone canonical into the lane, then restore the lane's own metadata.
    let meta = fs::read(lane.dir.join(".grove-meta.json")).ok();
    crate::seed::clone_tree(canonical, &lane.dir)?;
    if let Some(meta) = meta {
        write_atomic(&lane.dir.join(".grove-meta.json"), &meta)?;
    }
    Ok(true)
}

/// Publish a warmed lane as the canonical. Holds the canonical's lock exclusive, so
/// only one promote runs at a time and no seed reads it mid-rewrite.
pub fn promote(root: &Path, lane: &Lane, canonical: &Path) -> Result<()> {
    let lock = canonical_lock(root, canonical)?;
    lock.lock_exclusive()
        .context("exclusive-locking canonical for promote")?;
    crate::seed::clone_tree(&lane.dir, canonical)
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

fn lane_meta(dir: &Path) -> Option<LaneMeta> {
    serde_json::from_slice(&fs::read(dir.join(".grove-meta.json")).ok()?).ok()
}

/// When the lane for `(workspace, toolchain)` was last built in, if it exists. Every
/// `acquire` refreshes it, so the worktree pool reads it as an activity heartbeat to
/// decide when a worktree has gone idle long enough to be abandoned.
pub fn lane_last_used(root: &Path, workspace: &str, toolchain: &str) -> Option<u64> {
    lane_meta(&root.join("lanes").join(lane_id(workspace, toolchain))).map(|m| m.last_used)
}

fn lanes(root: &Path) -> Vec<PathBuf> {
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
fn try_own(root: &Path, id: &str) -> Option<File> {
    let file = File::create(lock_path(root, id)).ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

/// Remove a lane's directory. The caller must hold the lane's lock across this call.
/// Returns whether the directory is gone afterward. The lock file is deliberately
/// left in place: unlinking it lets one process keep a lock on the old inode while
/// another locks a freshly created one for the same lane (split-brain ownership).
fn remove_lane_dir(dir: &Path) -> bool {
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
        if let Some(_lock) = try_own(root, &id) {
            if remove_lane_dir(&dir) {
                reclaimed.push(id);
            }
        }
    }
    reclaimed
}

fn watermark_floor(root: &Path) -> u64 {
    let total = fs2::total_space(root).unwrap_or(0);
    (total / 10).max(MIN_FREE_FLOOR)
}

/// Evict whole lanes, least-recently-used first, until real free disk clears the
/// watermark. Skips in-use lanes and re-measures free disk after each removal
/// (deleting a copy-on-write clone can free nothing until the last sharer dies).
pub fn enforce_watermark(root: &Path) -> Vec<String> {
    let floor = watermark_floor(root);
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
                let last = lane_meta(&dir).map(|m| m.last_used).unwrap_or(0);
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
            break; // nothing evictable (all in use or undeletable)
        }
    }
    evicted
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
    pub size_bytes: u64,
    pub last_used: u64,
}

pub fn status(root: &Path) -> Status {
    let lanes: Vec<LaneStatus> = lanes(root)
        .into_iter()
        .map(|dir| {
            let id = dir.file_name().unwrap().to_string_lossy().into_owned();
            let meta = lane_meta(&dir);
            LaneStatus {
                id,
                workspace: meta.as_ref().map(|m| m.workspace.clone()),
                size_bytes: tree_size(&dir),
                last_used: meta.map(|m| m.last_used).unwrap_or(0),
            }
        })
        .collect();
    Status {
        root: root.display().to_string(),
        free_bytes: fs2::available_space(root).unwrap_or(0),
        floor_bytes: watermark_floor(root),
        lane_count: lanes.len(),
        lanes,
    }
}
