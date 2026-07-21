//! Evidence-based removal of build lanes superseded by shared warm state.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::lane::{LaneMeta, UNVERIFIED_BOOTSTRAP_TAG};
use super::{
    Lane, canonical_dir, canonical_lock, lane_meta, lanes, now_secs, remove_lane_dir, try_own,
    write_atomic,
};

const BOOTSTRAP_SUCCESS: &str = ".grove-bootstrap-success.json";
const CANONICAL_PUBLICATION: &str = ".grove-publication.json";
static PUBLICATION_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize, Serialize)]
struct BootstrapSuccess {
    schema_version: u32,
    workspace: String,
    toolchain: String,
    policy_sha256: String,
}

#[derive(Deserialize, Serialize)]
struct Publication {
    schema_version: u32,
    canonical: String,
    policy_sha256: String,
    published_at: u64,
    nonce: String,
}

#[derive(Deserialize, Serialize)]
struct CanonicalPublication {
    schema_version: u32,
    nonce: String,
}

type BootstrapKey = (String, String, String);
type Record = (PathBuf, String, LaneMeta);

fn bootstrap(meta: &LaneMeta) -> BootstrapSuccess {
    BootstrapSuccess {
        schema_version: 2,
        workspace: meta.workspace.clone(),
        toolchain: meta.toolchain.clone(),
        policy_sha256: meta.policy_sha256.clone(),
    }
}

fn success(dir: &Path, meta: &LaneMeta) -> bool {
    let Some(marker) = fs::read(dir.join(BOOTSTRAP_SUCCESS))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BootstrapSuccess>(&bytes).ok())
    else {
        return false;
    };
    let expected = bootstrap(meta);
    marker.schema_version == expected.schema_version
        && marker.workspace == expected.workspace
        && marker.toolchain == expected.toolchain
        && marker.policy_sha256 == expected.policy_sha256
}

fn publication_path(root: &Path, canonical: &Path) -> PathBuf {
    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    root.join("canonical-publication")
        .join(format!("{name}.json"))
}

pub(super) fn published_locked(root: &Path, canonical: &Path) -> bool {
    if !canonical.is_dir() {
        return false;
    }
    let Some(meta) = fs::read(publication_path(root, canonical))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Publication>(&bytes).ok())
    else {
        return false;
    };
    let Some(internal) = fs::read(canonical.join(CANONICAL_PUBLICATION))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CanonicalPublication>(&bytes).ok())
    else {
        return false;
    };
    meta.schema_version == 1
        && internal.schema_version == 1
        && meta.nonce == internal.nonce
        && meta.published_at > 0
        && !meta.policy_sha256.is_empty()
        && canonical
            .file_name()
            .is_some_and(|name| name == meta.canonical.as_str())
}

pub(super) fn published(root: &Path, canonical: &Path) -> bool {
    let Ok(lock) = canonical_lock(root, canonical) else {
        return false;
    };
    if lock.lock_shared().is_err() {
        return false;
    }
    published_locked(root, canonical)
}

pub(super) fn unpublish(root: &Path, canonical: &Path) -> Result<()> {
    match fs::remove_file(publication_path(root, canonical)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("clearing canonical publication"),
    }
    Ok(())
}

pub(super) fn publish(root: &Path, lane: &Lane, canonical: &Path) -> Result<()> {
    let canonical_name = canonical
        .file_name()
        .context("canonical path has no name")?
        .to_string_lossy()
        .into_owned();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let nonce = format!(
        "{}-{nanos}-{}",
        std::process::id(),
        PUBLICATION_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let internal = CanonicalPublication {
        schema_version: 1,
        nonce: nonce.clone(),
    };
    write_atomic(
        &canonical.join(CANONICAL_PUBLICATION),
        &serde_json::to_vec(&internal)?,
    )?;
    let publication = Publication {
        schema_version: 1,
        canonical: canonical_name,
        policy_sha256: lane.policy_sha256.clone(),
        published_at: now_secs(),
        nonce,
    };
    write_atomic(
        &publication_path(root, canonical),
        &serde_json::to_vec(&publication)?,
    )
}

/// Clear bootstrap evidence before a command can mutate its lane.
pub fn prepare(lane: &Lane) -> Result<()> {
    let Some(meta) = lane_meta(&lane.dir) else {
        return Ok(());
    };
    if meta.tag.as_deref() != Some(UNVERIFIED_BOOTSTRAP_TAG) {
        return Ok(());
    }
    match fs::remove_file(lane.dir.join(BOOTSTRAP_SUCCESS)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("clearing bootstrap success"),
    }
}

/// Atomically record that a Grove-routed Cargo command completed successfully.
pub fn succeed(lane: &Lane) -> Result<()> {
    let Some(meta) = lane_meta(&lane.dir) else {
        return Ok(());
    };
    if meta.tag.as_deref() != Some(UNVERIFIED_BOOTSTRAP_TAG) {
        return Ok(());
    }
    write_atomic(
        &lane.dir.join(BOOTSTRAP_SUCCESS),
        &serde_json::to_vec(&bootstrap(&meta))?,
    )
}

fn successful_bootstraps(root: &Path, records: &[Record]) -> (BTreeSet<BootstrapKey>, Vec<File>) {
    let mut keys = BTreeSet::new();
    let mut locks = Vec::new();
    for (dir, id, meta) in records {
        if meta.tag.as_deref() != Some(UNVERIFIED_BOOTSTRAP_TAG) {
            continue;
        }
        let Some(lock) = try_own(root, id) else {
            continue;
        };
        if success(dir, meta) {
            keys.insert((
                meta.workspace.clone(),
                meta.toolchain.clone(),
                meta.policy_sha256.clone(),
            ));
            locks.push(lock);
        }
    }
    (keys, locks)
}

fn regular_redundant(meta: &LaneMeta, bootstraps: &BTreeSet<BootstrapKey>) -> bool {
    let tag = meta.tag.as_deref().unwrap_or_default();
    if !tag.is_empty() {
        return false;
    }
    bootstraps.contains(&(
        meta.workspace.clone(),
        meta.toolchain.clone(),
        meta.policy_sha256.clone(),
    ))
}

pub(super) fn reclaim(root: &Path) -> Vec<String> {
    reclaim_after(root, || {})
}

pub(super) fn reclaim_after(root: &Path, mut published: impl FnMut()) -> Vec<String> {
    let records = lanes(root)
        .into_iter()
        .filter_map(|dir| {
            let id = dir.file_name()?.to_string_lossy().into_owned();
            let meta = lane_meta(&dir)?;
            Some((dir, id, meta))
        })
        .collect::<Vec<_>>();
    let (bootstraps, bootstrap_locks) = successful_bootstraps(root, &records);
    let mut reclaimed = Vec::new();
    for (dir, id, meta) in &records {
        if regular_redundant(meta, &bootstraps)
            && let Some(_lock) = try_own(root, id)
            && remove_lane_dir(dir)
        {
            reclaimed.push(id.clone());
        }
    }
    drop(bootstrap_locks);
    for (dir, id, meta) in records {
        if meta.tag.as_deref() != Some(UNVERIFIED_BOOTSTRAP_TAG) {
            continue;
        }
        let repo = crate::project::repo_identity(Path::new(&meta.workspace));
        let canonical = canonical_dir(root, &repo, &meta.toolchain);
        let Ok(lock) = canonical_lock(root, &canonical) else {
            continue;
        };
        if lock.lock_shared().is_ok() && published_locked(root, &canonical) {
            published();
            if let Some(_lane) = try_own(root, &id)
                && remove_lane_dir(&dir)
            {
                reclaimed.push(id);
            }
        }
    }
    reclaimed
}

pub(super) fn forget(root: &Path, canonical: &Path) {
    let _ = fs::remove_file(publication_path(root, canonical));
}
