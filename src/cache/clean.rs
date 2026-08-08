//! Cache hygiene under `$GROVE_CACHE`.
//!
//! Defaults keep one development cycle's worth of artifacts:
//! - After every successful build, drop agent lanes that are not live worktrees.
//! - Cap the whole cache (`GROVE_CACHE_MAX_GB`, default 40). Over budget: drop
//!   coldest idle lanes, then coldest other-repo seeds. The live checkout's seed
//!   is never auto-deleted.

use super::CacheHost;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Idle isolated lanes on *other* hosts (this host drops non-live lanes immediately).
const DEFAULT_LANE_TTL_DAYS: u64 = 1;
/// Hard ceiling for the whole cache root. `GROVE_CACHE_MAX_GB=0` disables it.
const DEFAULT_CACHE_MAX_GB: u64 = 40;
/// Reuse the last measured cache size this long before re-walking the root.
const SIZE_STAMP_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub struct CleanOpts {
    pub dry_run: bool,
    pub lanes: bool,
    /// Remove this host's seed (requires `force`). Manual only.
    pub seed: bool,
    pub force: bool,
    pub lane_ttl: Duration,
    /// Lane ids that must never be deleted (live checkouts of *this* workspace).
    pub protect_lane_keys: Vec<String>,
    /// Sweep idle lanes under every host (manual `--all`, and auto hygiene).
    pub all: bool,
    /// Evict until the cache root fits. `None` disables the ceiling.
    pub cache_max_bytes: Option<u64>,
    /// Host root whose seed must not be auto-evicted (current checkout).
    pub protect_host: Option<PathBuf>,
    /// The current checkout's own lane key: the only lane the last-resort
    /// budget stage will not evict.
    pub current_lane_key: Option<String>,
    /// Drop every non-protected lane on `protect_host` now (no TTL wait).
    pub drop_dead_lanes: bool,
}

impl CleanOpts {
    /// Defaults from env. Used by the `grove clean` CLI.
    pub fn from_env() -> Self {
        Self {
            dry_run: false,
            lanes: true,
            seed: false,
            force: false,
            lane_ttl: ttl_days("GROVE_LANE_TTL_DAYS", DEFAULT_LANE_TTL_DAYS),
            protect_lane_keys: Vec::new(),
            all: false,
            cache_max_bytes: cache_max_bytes(),
            protect_host: None,
            current_lane_key: None,
            drop_dead_lanes: false,
        }
    }

    /// What runs after every successful warm / check / build / test / exec.
    pub fn auto() -> Self {
        let mut o = Self::from_env();
        o.seed = false;
        o.all = true;
        o.drop_dead_lanes = true;
        o
    }
}

fn cache_max_bytes() -> Option<u64> {
    let gb = match std::env::var("GROVE_CACHE_MAX_GB") {
        Ok(s) => s.parse::<u64>().unwrap_or(DEFAULT_CACHE_MAX_GB),
        Err(_) => DEFAULT_CACHE_MAX_GB,
    };
    (gb > 0).then(|| gb * 1024 * 1024 * 1024)
}

fn ttl_days(var: &str, default_days: u64) -> Duration {
    match std::env::var(var) {
        Ok(s) => match s.parse::<u64>() {
            Ok(0) => Duration::MAX,
            Ok(d) => Duration::from_secs(d.saturating_mul(24 * 3600)),
            Err(_) => Duration::from_secs(default_days * 24 * 3600),
        },
        Err(_) => Duration::from_secs(default_days * 24 * 3600),
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CleanReport {
    pub dry_run: bool,
    pub lanes_removed: usize,
    pub worktree_dirs_removed: usize,
    pub seeds_removed: usize,
    pub bytes_reclaimed: u64,
    pub paths: Vec<String>,
    /// Still over budget after every safe eviction (should be rare: live seed alone).
    pub over_budget_bytes: u64,
}

impl CleanReport {
    pub fn touched(&self) -> bool {
        !self.paths.is_empty()
    }

    /// Compatibility for callers that only care whether the current seed went.
    pub fn seed_removed(&self) -> bool {
        self.seeds_removed > 0
    }
}

/// Hardlink identity when the platform exposes a stable identity and link count.
#[cfg(unix)]
fn hardlink_identity(_path: &Path, meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (meta.nlink() > 1).then_some((meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn hardlink_identity(path: &Path, _meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = File::open(path).ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // Safety: `file` keeps the handle valid and `info` is a writable C-layout value.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if ok == 0 || info.nNumberOfLinks <= 1 {
        return None;
    }
    Some((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(not(any(unix, windows)))]
fn hardlink_identity(_path: &Path, _meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Bytes under `path`, counting each inode once (hardlinked lane/seed artifacts).
fn dir_size(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry_path);
            } else if file_type.is_file() {
                let Ok(meta) = entry.metadata() else { continue };
                if let Some(identity) = hardlink_identity(&entry_path, &meta)
                    && !seen.insert(identity)
                {
                    continue;
                }
                total += meta.len();
            }
        }
    }
    total
}

fn mtime(path: &Path) -> Option<SystemTime> {
    let access = path.join(".grove-access");
    if access.is_file() {
        return access.metadata().and_then(|m| m.modified()).ok();
    }
    path.metadata().and_then(|m| m.modified()).ok()
}

fn is_idle(path: &Path, ttl: Duration, now: SystemTime) -> bool {
    if ttl == Duration::MAX {
        return false;
    }
    let Some(m) = mtime(path) else {
        return false;
    };
    now.duration_since(m).map(|d| d >= ttl).unwrap_or(false)
}

fn remove_tree(
    path: &Path,
    dry_run: bool,
    report: &mut CleanReport,
    seen: &mut HashSet<(u64, u64)>,
) -> Result<()> {
    let bytes = dir_size(path, seen);
    report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
    report.paths.push(path.display().to_string());
    if !dry_run {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn lane_probe(lane: &Path) -> PathBuf {
    let t = lane.join("target");
    if t.is_dir() { t } else { lane.to_path_buf() }
}

/// `{slug}/{toolchain}/lanes` under the cache root.
fn all_lane_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for host in all_host_roots(root) {
        let lanes = host.join("lanes");
        if lanes.is_dir() {
            out.push(lanes);
        }
    }
    out
}

/// `{slug}/{toolchain}` host roots (skip legacy `stable-*` dirs and dotfiles).
fn all_host_roots(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(slugs) = fs::read_dir(root) else {
        return out;
    };
    for slug in slugs.flatten() {
        let name = slug.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("stable-") || name.starts_with('.') {
            continue;
        }
        let Ok(tcs) = fs::read_dir(slug.path()) else {
            continue;
        };
        for tc in tcs.flatten() {
            let host = tc.path();
            if host.join("seed").is_dir() || host.join("lanes").is_dir() {
                out.push(host);
            }
        }
    }
    out
}

fn all_lanes_by_age(lane_dirs: &[PathBuf], protect: &[String]) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    for lanes in lane_dirs {
        let Ok(entries) = fs::read_dir(lanes) else {
            continue;
        };
        for ent in entries.flatten() {
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let key = ent.file_name().to_string_lossy().into_owned();
            if protect.iter().any(|p| p == &key) {
                continue;
            }
            let path = ent.path();
            if let Some(m) = mtime(&lane_probe(&path)) {
                out.push((path, m));
            }
        }
    }
    out
}

/// Other hosts' seed targets, coldest first (by `.grove-access` or mtime).
fn other_seeds_by_age(root: &Path, protect_host: Option<&Path>) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    for host in all_host_roots(root) {
        if protect_host.is_some_and(|p| same_path(p, &host)) {
            continue;
        }
        let seed = host.join("seed");
        if !seed.is_dir() {
            continue;
        }
        let probe = seed.join("target");
        let probe = if probe.is_dir() { probe } else { seed.clone() };
        if let Some(m) = mtime(&probe) {
            out.push((seed, m));
        }
    }
    out.sort_by_key(|(_, m)| *m);
    out
}

fn same_path(a: &Path, b: &Path) -> bool {
    let ca = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

pub(crate) fn touch_access(target_dir: &Path) {
    let _ = fs::write(target_dir.join(".grove-access"), b"");
}

fn size_stamp_path(root: &Path) -> PathBuf {
    root.join(".total-size")
}

fn read_size_stamp(root: &Path, now: SystemTime) -> Option<u64> {
    let text = fs::read_to_string(size_stamp_path(root)).ok()?;
    let mut lines = text.lines();
    let bytes: u64 = lines.next()?.trim().parse().ok()?;
    let secs: u64 = lines.next()?.trim().parse().ok()?;
    let stamped = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    (now.duration_since(stamped).ok()? < SIZE_STAMP_TTL).then_some(bytes)
}

fn write_size_stamp(root: &Path, bytes: u64, now: SystemTime) {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = fs::write(size_stamp_path(root), format!("{bytes}\n{secs}\n"));
}

/// Legacy fixed-path source views; no longer created.
fn remove_stable_views(
    root: &Path,
    dry_run: bool,
    report: &mut CleanReport,
    seen: &mut HashSet<(u64, u64)>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(());
    };
    for ent in entries.flatten() {
        if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if ent.file_name().to_string_lossy().starts_with("stable-") {
            remove_tree(&ent.path(), dry_run, report, seen)?;
        }
    }
    Ok(())
}

impl CacheHost {
    /// Post-build hygiene: dead lanes + global budget (see [`CleanOpts::auto`]).
    pub fn hygiene(&self, workspace: &Path) -> Result<CleanReport> {
        let mut opts = CleanOpts::auto();
        opts.protect_host = Some(self.host_root().to_path_buf());
        opts.current_lane_key = Some(super::worktree_lane_key(workspace));
        match live_lane_keys(workspace) {
            Ok(mut keys) => {
                keys.push(super::worktree_lane_key(workspace));
                opts.protect_lane_keys = keys;
            }
            Err(e) => {
                // Still enforce budget / TTL; just cannot protect by live worktree list.
                eprintln!(
                    "grove: hygiene: live worktrees unknown ({e:#}); protecting current only"
                );
                opts.protect_lane_keys = vec![super::worktree_lane_key(workspace)];
            }
        }
        self.clean(&opts)
    }

    /// Reclaim storage per `opts`.
    pub fn clean(&self, opts: &CleanOpts) -> Result<CleanReport> {
        let mut report = CleanReport {
            dry_run: opts.dry_run,
            ..Default::default()
        };
        let now = SystemTime::now();
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        let root = super::cache_root();

        // 0. Legacy stable-src views are dead weight; reclaim them.
        remove_stable_views(&root, opts.dry_run, &mut report, &mut seen)?;

        // 1. This host: drop agent lanes that are not live worktrees (no TTL wait).
        if opts.drop_dead_lanes {
            let lanes = self.host_root().join("lanes");
            if lanes.is_dir() {
                for ent in fs::read_dir(&lanes)?.flatten() {
                    if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let key = ent.file_name().to_string_lossy().into_owned();
                    if opts.protect_lane_keys.iter().any(|p| p == &key) {
                        continue;
                    }
                    remove_tree(&ent.path(), opts.dry_run, &mut report, &mut seen)?;
                    report.lanes_removed += 1;
                }
            }
        }

        // 2. Idle lanes (TTL): this host, or every host with `--all` / auto.
        let lane_dirs: Vec<PathBuf> = if opts.all {
            all_lane_dirs(&root)
        } else {
            vec![self.host_root().join("lanes")]
        };

        if opts.lanes && opts.lane_ttl != Duration::MAX {
            for lanes in &lane_dirs {
                if !lanes.is_dir() {
                    continue;
                }
                // Dead-lane pass already emptied non-live dirs on this host.
                if opts.drop_dead_lanes && same_path(lanes, &self.host_root().join("lanes")) {
                    continue;
                }
                for ent in fs::read_dir(lanes)?.flatten() {
                    if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let key = ent.file_name().to_string_lossy().into_owned();
                    if opts.protect_lane_keys.iter().any(|p| p == &key) {
                        continue;
                    }
                    let path = ent.path();
                    if is_idle(&lane_probe(&path), opts.lane_ttl, now) {
                        remove_tree(&path, opts.dry_run, &mut report, &mut seen)?;
                        report.lanes_removed += 1;
                    }
                }
            }
        }

        // 3. Budget: coldest idle lanes, then coldest other-repo seeds, then this
        //    host's coldest lanes. The full-root walk is cached in a stamp so
        //    steady-state builds skip the O(cache) stat sweep.
        if let Some(budget) = opts.cache_max_bytes {
            let stamped = if opts.dry_run {
                None
            } else {
                read_size_stamp(&root, now)
            };
            let mut total = match stamped {
                Some(bytes) => bytes.saturating_sub(report.bytes_reclaimed),
                None => dir_size(&root, &mut HashSet::new()),
            };
            if total > budget {
                let mut lanes = all_lanes_by_age(&all_lane_dirs(&root), &opts.protect_lane_keys);
                lanes.sort_by_key(|(_, m)| *m);
                for (path, _) in lanes {
                    if total <= budget {
                        break;
                    }
                    let before = report.bytes_reclaimed;
                    remove_tree(&path, opts.dry_run, &mut report, &mut seen)?;
                    report.lanes_removed += 1;
                    total = total.saturating_sub(report.bytes_reclaimed - before);
                }
            }
            if total > budget {
                let protect = opts
                    .protect_host
                    .as_deref()
                    .unwrap_or_else(|| self.host_root());
                for (seed, _) in other_seeds_by_age(&root, Some(protect)) {
                    if total <= budget {
                        break;
                    }
                    let before = report.bytes_reclaimed;
                    // Drop the whole host (seed + any leftover lanes) for a cold repo.
                    let host = seed.parent().unwrap_or(&seed).to_path_buf();
                    let drop = if host.join("seed").is_dir() {
                        host
                    } else {
                        seed
                    };
                    remove_tree(&drop, opts.dry_run, &mut report, &mut seen)?;
                    report.seeds_removed += 1;
                    total = total.saturating_sub(report.bytes_reclaimed - before);
                }
            }
            // Last resort: this host's own lanes, coldest first. A live worktree
            // just rebuilds its lane from the seed; only the current checkout's
            // lane and the protected seed stay untouchable.
            if total > budget {
                let own_lanes = self.host_root().join("lanes");
                if own_lanes.is_dir() {
                    let protect: Vec<String> = opts.current_lane_key.iter().cloned().collect();
                    let mut lanes = all_lanes_by_age(std::slice::from_ref(&own_lanes), &protect);
                    lanes.sort_by_key(|(_, m)| *m);
                    for (path, _) in lanes {
                        if total <= budget {
                            break;
                        }
                        let before = report.bytes_reclaimed;
                        remove_tree(&path, opts.dry_run, &mut report, &mut seen)?;
                        report.lanes_removed += 1;
                        total = total.saturating_sub(report.bytes_reclaimed - before);
                    }
                }
            }
            report.over_budget_bytes = total.saturating_sub(budget);
            if !opts.dry_run {
                write_size_stamp(&root, total, now);
            }
        }

        // 4. Manual seed wipe for *this* host only.
        if opts.seed {
            if !opts.force {
                anyhow::bail!("refusing to remove seed without --force");
            }
            let seed = self.seed_target();
            // seed_target is …/seed/target; remove the seed/ directory.
            let seed_dir = seed.parent().unwrap_or(&seed).to_path_buf();
            if seed_dir.is_dir() {
                remove_tree(&seed_dir, opts.dry_run, &mut report, &mut seen)?;
                report.seeds_removed += 1;
            } else if seed.is_dir() {
                remove_tree(&seed, opts.dry_run, &mut report, &mut seen)?;
                report.seeds_removed += 1;
            }
        }

        Ok(report)
    }

    /// Remove grove-stamped dirs under the worktree parent that are not in `git worktree list`.
    pub fn clean_orphan_worktrees(&self, workspace: &Path, dry_run: bool) -> Result<CleanReport> {
        let mut report = CleanReport {
            dry_run,
            ..Default::default()
        };
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        let live = live_worktree_paths(workspace)?;
        let root = crate::worktree::worktree_parent(workspace);
        if !orphan_root_ok(workspace, &root) {
            anyhow::bail!(
                "orphan clean refused: worktree root {} is not under GROVE_CACHE or the repo worktree parent",
                root.display()
            );
        }
        if !root.is_dir() {
            return Ok(report);
        }
        for ent in fs::read_dir(&root)?.flatten() {
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = ent.path();
            if !path.join(crate::worktree::WORKTREE_STAMP).is_file() {
                continue;
            }
            let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let still_live = live.iter().any(|l| {
                fs::canonicalize(l).map(|c| c == canon).unwrap_or(false) || Path::new(l) == path
            });
            if still_live {
                continue;
            }
            remove_tree(&path, dry_run, &mut report, &mut seen)?;
            report.worktree_dirs_removed += 1;
        }
        Ok(report)
    }
}

fn orphan_root_ok(workspace: &Path, root: &Path) -> bool {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let cache = super::cache_root();
    let cache = fs::canonicalize(&cache).unwrap_or(cache);
    if root.starts_with(&cache) {
        return true;
    }
    let sibling = crate::worktree::default_worktree_parent(workspace);
    let sibling = fs::canonicalize(&sibling).unwrap_or(sibling);
    root == sibling || root.starts_with(&sibling)
}

fn live_worktree_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    crate::git::worktree_paths(workspace)
        .map_err(|e| anyhow::anyhow!("git worktree list failed (refuse orphan clean): {e:#}"))
}

/// Lane keys for every live worktree. Errors if `git worktree list` fails.
pub fn live_lane_keys(workspace: &Path) -> Result<Vec<String>> {
    Ok(live_worktree_paths(workspace)?
        .iter()
        .map(|p| super::worktree_lane_key(p))
        .collect())
}

pub fn format_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n < K {
        format!("{n:.0} B")
    } else if n < K * K {
        format!("{:.1} KiB", n / K)
    } else if n < K * K * K {
        format!("{:.1} MiB", n / (K * K))
    } else {
        format!("{:.2} GiB", n / (K * K * K))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn idle_respects_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lane");
        fs::create_dir_all(&p).unwrap();
        touch_access(&p);
        let now = SystemTime::now();
        assert!(!is_idle(&p, Duration::from_secs(3600), now));
    }

    #[test]
    fn format_bytes_smoke() {
        assert!(format_bytes(500).contains('B'));
        assert!(format_bytes(5_000_000).contains("MiB"));
    }

    #[test]
    fn orphan_clean_skips_unstamped_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let stranger = root.join("unrelated-crate");
        fs::create_dir_all(&stranger).unwrap();
        fs::write(stranger.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let stamped = root.join("old-agent");
        fs::create_dir_all(&stamped).unwrap();
        fs::write(stamped.join(crate::worktree::WORKTREE_STAMP), "agent=old\n").unwrap();
        fs::write(stamped.join("Cargo.toml"), "[package]\nname=\"y\"\n").unwrap();

        let would_remove = |p: &Path| p.join(crate::worktree::WORKTREE_STAMP).is_file();
        assert!(!would_remove(&stranger));
        assert!(would_remove(&stamped));
    }
}
