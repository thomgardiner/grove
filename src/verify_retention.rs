//! Graph-based garbage collection for durable verification evidence.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use super::receipt::{Receipt, Run};
use crate::task;

#[path = "verify_retention_portable.rs"]
mod retention_portable;

struct StoredRun {
    slug: String,
    path: PathBuf,
    run: Run,
}

type RunIds = BTreeSet<(String, String)>;
type SnapshotRefs = BTreeMap<String, BTreeSet<String>>;

/// Hold evidence publication while a normal verification profile writes its receipts and
/// completion record. GC uses the same lock non-blockingly, so it cannot collect an
/// in-progress profile's receipts before its run record exists.
pub(super) fn lock(root: &Path) -> Result<File> {
    let path = lock_path(root)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// Reclaim evidence that no task verifier or portable clean-checkout lookup can select.
pub(super) fn reclaim(root: &Path) -> Vec<String> {
    let Some(_lock) = try_lock(root) else {
        return Vec::new();
    };
    let unsafe_repos = unsafe_repositories(root);
    let runs = read_runs(root);
    let latest = latest_runs(&runs);
    let mut protected = release_runs(root);
    protected.extend(retention_portable::runs(root, &runs));
    let mut reclaimed = Vec::new();
    let retained = prune_runs(&runs, &latest, &protected, &unsafe_repos, &mut reclaimed);
    let mut snapshots = task_snapshots(root);
    prune_receipts(
        root,
        &retained,
        &unsafe_repos,
        &mut snapshots,
        &mut reclaimed,
    );
    prune_snapshots(root, &snapshots, &unsafe_repos, &mut reclaimed);
    reclaimed
}

fn lock_path(root: &Path) -> Result<PathBuf> {
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    Ok(locks.join("verification-evidence.lock"))
}

fn try_lock(root: &Path) -> Option<File> {
    let path = lock_path(root).ok()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

fn repositories(root: &Path, name: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(root.join(name)) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| entry)
        })
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect()
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect()
}

fn parse<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn read_runs(root: &Path) -> Vec<StoredRun> {
    repositories(root, "verification-runs")
        .into_iter()
        .flat_map(|(slug, dir)| {
            json_files(&dir).into_iter().filter_map(move |path| {
                parse(&path).map(|run| StoredRun {
                    slug: slug.clone(),
                    path,
                    run,
                })
            })
        })
        .collect()
}

/// A malformed record can be an interrupted write, an incompatible schema, or evidence
/// Grove does not yet understand. Preserve that repository's graph rather than guessing.
fn unsafe_repositories(root: &Path) -> BTreeSet<String> {
    let mut repos = BTreeSet::new();
    mark_malformed::<Run>(root, "verification-runs", &mut repos);
    mark_malformed::<task::Task>(root, "tasks", &mut repos);
    mark_malformed::<Receipt>(root, "receipts", &mut repos);
    repos
}

fn mark_malformed<T: serde::de::DeserializeOwned>(
    root: &Path,
    kind: &str,
    repos: &mut BTreeSet<String>,
) {
    for (slug, dir) in repositories(root, kind) {
        if json_files(&dir)
            .into_iter()
            .any(|path| parse::<T>(&path).is_none())
        {
            repos.insert(slug);
        }
    }
}

/// Frozen release receipts have a dedicated lane tag. Keep their complete run forever:
/// a later ad-hoc taskless verification must not erase published release provenance.
fn release_runs(root: &Path) -> RunIds {
    let mut pinned = RunIds::new();
    for (slug, dir) in repositories(root, "receipts") {
        for path in json_files(&dir) {
            if let Some(receipt) = parse::<Receipt>(&path)
                && receipt.task_id.is_none()
                && receipt.lane.tag.starts_with("release-freeze-")
                && !receipt.run_id.is_empty()
            {
                pinned.insert((slug.clone(), receipt.run_id));
            }
        }
    }
    pinned
}

fn latest_runs(runs: &[StoredRun]) -> BTreeSet<usize> {
    let mut latest = BTreeMap::<(String, Option<String>, String), usize>::new();
    for (index, stored) in runs.iter().enumerate() {
        if stored.run.schema_version != 1 {
            continue;
        }
        let key = (
            stored.slug.clone(),
            stored.run.task_id.clone(),
            stored.run.profile.clone(),
        );
        if latest
            .get(&key)
            .is_none_or(|current| newer(&stored.run, &runs[*current].run))
        {
            latest.insert(key, index);
        }
    }
    latest.into_values().collect()
}

fn newer(candidate: &Run, current: &Run) -> bool {
    (candidate.completed_at_nanos, &candidate.run_id)
        > (current.completed_at_nanos, &current.run_id)
}

fn prune_runs(
    runs: &[StoredRun],
    latest: &BTreeSet<usize>,
    pinned: &RunIds,
    unsafe_repos: &BTreeSet<String>,
    reclaimed: &mut Vec<String>,
) -> RunIds {
    let mut retained = pinned.clone();
    for (index, stored) in runs.iter().enumerate() {
        let id = (stored.slug.clone(), stored.run.run_id.clone());
        if unsafe_repos.contains(&stored.slug)
            || stored.run.schema_version != 1
            || latest.contains(&index)
            || pinned.contains(&id)
        {
            retained.insert(id);
        } else {
            remove(&stored.path, "run", &stored.slug, reclaimed);
        }
    }
    retained
}

fn task_snapshots(root: &Path) -> SnapshotRefs {
    let mut references = SnapshotRefs::new();
    for (slug, dir) in repositories(root, "tasks") {
        for path in json_files(&dir) {
            if let Some(reference) = parse::<task::Task>(&path).and_then(|task| task.scope_snapshot)
            {
                references
                    .entry(slug.clone())
                    .or_default()
                    .insert(reference.sha256);
            }
        }
    }
    references
}

fn prune_receipts(
    root: &Path,
    retained: &RunIds,
    unsafe_repos: &BTreeSet<String>,
    snapshots: &mut SnapshotRefs,
    reclaimed: &mut Vec<String>,
) {
    for (slug, dir) in repositories(root, "receipts") {
        if unsafe_repos.contains(&slug) {
            continue;
        }
        for path in json_files(&dir) {
            let Some(receipt) = parse::<Receipt>(&path) else {
                continue;
            };
            if retained.contains(&(slug.clone(), receipt.run_id.clone())) {
                retain_receipt_snapshots(snapshots, &slug, &receipt);
            } else {
                remove(&path, "receipt", &slug, reclaimed);
            }
        }
        remove_empty(&dir);
    }
}

fn retain_receipt_snapshots(references: &mut SnapshotRefs, slug: &str, receipt: &Receipt) {
    let Some(evidence) = receipt.evidence.as_ref() else {
        return;
    };
    let references = references.entry(slug.to_string()).or_default();
    references.insert(evidence.input.sha256.clone());
    references.insert(evidence.output.sha256.clone());
}

fn prune_snapshots(
    root: &Path,
    references: &SnapshotRefs,
    unsafe_repos: &BTreeSet<String>,
    reclaimed: &mut Vec<String>,
) {
    for (slug, dir) in repositories(root, "snapshots") {
        if unsafe_repos.contains(&slug) {
            continue;
        }
        let retained = references.get(&slug);
        for path in json_files(&dir) {
            let digest = path.file_stem().and_then(|stem| stem.to_str());
            if !digest.is_some_and(|digest| retained.is_some_and(|set| set.contains(digest))) {
                remove(&path, "snapshot", &slug, reclaimed);
            }
        }
        remove_empty(&dir);
    }
}

fn remove(path: &Path, kind: &str, slug: &str, reclaimed: &mut Vec<String>) {
    if fs::remove_file(path).is_ok() || !path.exists() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown");
        reclaimed.push(format!("{kind}:{slug}/{name}"));
    }
}

fn remove_empty(dir: &Path) {
    let _ = fs::remove_dir(dir);
}
