use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cache;

use super::MaterializationRecord;

static GENERATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The on-disk record that makes a worktree grove-managed. Its existence is what
/// authorizes reap to remove the worktree.
#[derive(Serialize, Deserialize, Clone)]
pub struct Lease {
    pub workspace: String,
    pub branch: String,
    pub agent: String,
    pub toolchain: String,
    /// The repo's shared git dir; its parent is the main worktree git commands run from.
    pub repo: String,
    pub created_at: u64,
    /// Unique durable identity for this acquisition, independent of wall-clock resolution.
    #[serde(default)]
    pub generation: String,
    /// Last explicit or task-driven renewal. Old leases fall back to `created_at`.
    #[serde(default)]
    pub last_activity: u64,
    /// The commit the worktree branched from, so `squash` knows the fork point.
    #[serde(default)]
    pub base_oid: String,
    /// Measured sparse/full checkout state. Legacy leases omit it and remain full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialization: Option<MaterializationRecord>,
}

pub(super) fn generation() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = GENERATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:032x}-{:08x}-{sequence:016x}", std::process::id())
}

fn leases_dir(root: &Path) -> PathBuf {
    root.join("leases")
}

fn lease_path(root: &Path, workspace: &str, toolchain: &str) -> PathBuf {
    leases_dir(root).join(format!("{}.json", cache::lane_id(workspace, toolchain)))
}

pub(super) fn leases(root: &Path) -> Vec<(PathBuf, Lease)> {
    fs::read_dir(leases_dir(root))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| {
            match fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            {
                Some(lease) => Some((path, lease)),
                None => {
                    eprintln!(
                        "grove: preserving unreadable lease {}; cleanup authority is ambiguous",
                        path.display()
                    );
                    None
                }
            }
        })
        .collect()
}

fn authority_leases(root: &Path) -> Result<Vec<(PathBuf, Lease)>> {
    let entries = match fs::read_dir(leases_dir(root)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut leases = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let bytes =
            fs::read(&path).with_context(|| format!("reading grove lease {}", path.display()))?;
        let lease = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "parsing grove lease {}; preserving ambiguous cleanup authority",
                path.display()
            )
        })?;
        leases.push((path, lease));
    }
    Ok(leases)
}

pub(super) fn find_lease(root: &Path, workspace: &str) -> Result<Option<(PathBuf, Lease)>> {
    let mut matches = authority_leases(root)?
        .into_iter()
        .filter(|(_, lease)| lease.workspace == workspace);
    let found = matches.next();
    if matches.next().is_some() {
        bail!("multiple grove leases name {workspace}; refusing ambiguous authority")
    }
    Ok(found)
}

pub(super) fn containing(root: &Path, target: &Path) -> Result<Option<(PathBuf, Lease)>> {
    let target = cache::canonical_path(target);
    let mut matches = authority_leases(root)?.into_iter().filter(|(_, lease)| {
        target.starts_with(cache::canonical_path(Path::new(&lease.workspace)))
    });
    let found = matches.next();
    if matches.next().is_some() {
        bail!(
            "multiple grove leases contain {}; refusing ambiguous authority",
            target.display()
        )
    }
    Ok(found)
}

pub(super) fn write_lease(root: &Path, lease: &Lease) -> Result<()> {
    fs::create_dir_all(leases_dir(root))?;
    cache::write_atomic(
        &lease_path(root, &lease.workspace, &lease.toolchain),
        &serde_json::to_vec_pretty(lease)?,
    )
}

pub(super) fn activity(root: &Path, lease: &Lease) -> u64 {
    lease
        .created_at
        .max(lease.last_activity)
        .max(cache::workspace_last_used(root, &lease.workspace).unwrap_or(0))
}
