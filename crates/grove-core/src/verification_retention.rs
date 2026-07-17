//! Retention bookkeeping for durable verification evidence.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::{
    task,
    verification::{Receipt, Run},
};

pub type RunIds = BTreeSet<(String, String)>;
type SnapshotRefs = BTreeMap<String, BTreeSet<String>>;

struct StoredRun {
    slug: String,
    path: PathBuf,
    run: Run,
}

pub fn lock(root: &Path) -> Result<File> {
    let path = lock_path(root)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    FileExt::lock_shared(&file).with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

pub fn reclaim(root: &Path, portable: &RunIds) -> Vec<String> {
    let Some(_lock) = try_lock(root) else {
        return Vec::new();
    };
    let unsafe_repos = unsafe_repositories(root);
    let runs = read_runs(root);
    let mut protected = release_runs(root);
    protected.extend(portable.iter().cloned());
    let mut reclaimed = Vec::new();
    let retained = prune_runs(&runs, &protected, &unsafe_repos, &mut reclaimed);
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
    fs::read_dir(root.join(name))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect()
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
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

fn release_runs(root: &Path) -> RunIds {
    repositories(root, "receipts")
        .into_iter()
        .flat_map(|(slug, dir)| {
            json_files(&dir).into_iter().filter_map(move |path| {
                parse::<Receipt>(&path).and_then(|receipt| {
                    (receipt.task_id.is_none()
                        && receipt.lane.tag.starts_with("release-freeze-")
                        && !receipt.run_id.is_empty())
                    .then_some((slug.clone(), receipt.run_id))
                })
            })
        })
        .collect()
}

fn prune_runs(
    runs: &[StoredRun],
    protected: &RunIds,
    unsafe_repos: &BTreeSet<String>,
    reclaimed: &mut Vec<String>,
) -> RunIds {
    let mut latest = BTreeMap::<(String, Option<String>, String), usize>::new();
    for (index, stored) in runs
        .iter()
        .enumerate()
        .filter(|(_, stored)| stored.run.schema_version == 1)
    {
        let key = (
            stored.slug.clone(),
            stored.run.task_id.clone(),
            stored.run.profile.clone(),
        );
        if latest.get(&key).is_none_or(|current| {
            (stored.run.completed_at_nanos, &stored.run.run_id)
                > (
                    runs[*current].run.completed_at_nanos,
                    &runs[*current].run.run_id,
                )
        }) {
            latest.insert(key, index);
        }
    }
    let latest = latest.into_values().collect::<BTreeSet<_>>();
    let mut retained = protected.clone();
    for (index, stored) in runs.iter().enumerate() {
        let id = (stored.slug.clone(), stored.run.run_id.clone());
        if unsafe_repos.contains(&stored.slug)
            || stored.run.schema_version != 1
            || latest.contains(&index)
            || protected.contains(&id)
        {
            retained.insert(id);
        } else {
            remove(&stored.path, "run", &stored.slug, reclaimed);
        }
    }
    retained
}

fn task_snapshots(root: &Path) -> SnapshotRefs {
    repositories(root, "tasks")
        .into_iter()
        .flat_map(|(slug, dir)| {
            json_files(&dir).into_iter().filter_map(move |path| {
                parse::<task::Task>(&path).and_then(|task| {
                    task.scope_snapshot
                        .map(|reference| (slug.clone(), reference.sha256))
                })
            })
        })
        .fold(SnapshotRefs::new(), |mut references, (slug, digest)| {
            references.entry(slug).or_default().insert(digest);
            references
        })
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
                if let Some(evidence) = receipt.evidence {
                    let refs = snapshots.entry(slug.clone()).or_default();
                    refs.insert(evidence.input.sha256);
                    refs.insert(evidence.output.sha256);
                }
            } else {
                remove(&path, "receipt", &slug, reclaimed);
            }
        }
        let _ = fs::remove_dir(&dir);
    }
}

fn prune_snapshots(
    root: &Path,
    refs: &SnapshotRefs,
    unsafe_repos: &BTreeSet<String>,
    reclaimed: &mut Vec<String>,
) {
    for (slug, dir) in repositories(root, "snapshots") {
        if unsafe_repos.contains(&slug) {
            continue;
        }
        for path in json_files(&dir) {
            let digest = path.file_stem().and_then(|stem| stem.to_str());
            if !digest.is_some_and(|digest| refs.get(&slug).is_some_and(|set| set.contains(digest)))
            {
                remove(&path, "snapshot", &slug, reclaimed);
            }
        }
        let _ = fs::remove_dir(&dir);
    }
}

fn remove(path: &Path, kind: &str, slug: &str, reclaimed: &mut Vec<String>) {
    if fs::remove_file(path).is_ok() || !path.exists() {
        reclaimed.push(format!(
            "{kind}:{slug}/{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
        ));
    }
}
