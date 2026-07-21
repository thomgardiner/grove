//! Durable task lifecycle built on Grove's existing claim registry and tagged lanes.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, claim, git, project, snapshot};
use grove_core::task::SCHEMA_VERSION;
pub use grove_core::task::{
    CommandRecord, CommandState, Lifecycle, RecoveryRecord, Task, Verification,
};

const MIN_SUPPORTED_SCHEMA_VERSION: u32 = 1;

pub struct Begin<'a> {
    pub root: &'a Path,
    pub workspace: &'a Path,
    pub agent: String,
    pub description: String,
    pub scope: Vec<String>,
    /// Tasks sharing a claim group deliberately overlap without conflicting
    /// (N-version attempts at one order); outsiders still conflict.
    pub claim_group: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum BeginOutcome {
    Begun {
        task: Box<Task>,
    },
    Conflict {
        requested: Vec<String>,
        conflicts: Vec<claim::Claim>,
    },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn task_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", std::process::id())
}
fn dir(root: &Path, repo: &str) -> PathBuf {
    root.join("tasks").join(cache::repo_slug(repo))
}
fn path(root: &Path, repo: &str, id: &str) -> PathBuf {
    dir(root, repo).join(format!("{id}.json"))
}
pub(crate) fn write(root: &Path, task: &Task) -> Result<()> {
    cache::write_atomic(
        &path(root, &task.repo, &task.id),
        &serde_json::to_vec_pretty(task)?,
    )
}
fn read_records(root: &Path, repo: &str, quarantine: bool) -> Result<Vec<Task>> {
    let Ok(entries) = fs::read_dir(dir(root, repo)) else {
        return Ok(Vec::new());
    };
    let mut tasks = Vec::new();
    for path in entries.filter_map(|entry| entry.ok()).map(|e| e.path()) {
        if !path.extension().is_some_and(|ext| ext == "json") {
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        match serde_json::from_slice(&bytes) {
            Ok(task) => tasks.push(task),
            Err(error) if quarantine => claim::quarantine_corrupt(&path, &error)?,
            Err(error) => {
                bail!(
                    "malformed task record {} preserved during read-only recovery: {error}",
                    path.display()
                )
            }
        }
    }
    Ok(tasks)
}

pub(crate) fn records(root: &Path, repo: &str) -> Result<Vec<Task>> {
    read_records(root, repo, true)
}

/// A repository registry snapshot held stable until worktree cleanup finishes.
pub(crate) struct Blockers {
    ids: Vec<String>,
    _guard: fs::File,
}

impl Blockers {
    pub(crate) fn ids(&self) -> &[String] {
        &self.ids
    }
}

fn cleanup_record(path: &Path, repo: &str) -> Result<Option<Task>> {
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".json.corrupt"))
    {
        bail!(
            "quarantined task record {}; cleanup ownership is unknown",
            path.display()
        );
    }
    if !path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let task: Task = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "malformed task record {}; cleanup ownership is unknown",
            path.display()
        )
    })?;
    if !(MIN_SUPPORTED_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&task.schema_version) {
        bail!(
            "task record {} has unsupported schema {}; cleanup ownership is unknown",
            path.display(),
            task.schema_version
        );
    }
    if task.repo != repo {
        bail!(
            "task record {} belongs to a different repository; cleanup ownership is unknown",
            path.display()
        );
    }
    Ok(Some(task))
}

/// Lock one repository's task registry and return nonterminal tasks attached to the
/// canonical workspace. Malformed records fail closed and remain byte-for-byte intact.
pub(crate) fn blockers(root: &Path, repo: &str, workspace: &Path) -> Result<Blockers> {
    blockers_except(root, repo, workspace, None)
}

pub(crate) fn blockers_except(
    root: &Path,
    repo: &str,
    workspace: &Path,
    ignore: Option<&str>,
) -> Result<Blockers> {
    let guard = claim::registry_lock(root, repo)?;
    let target = cache::canonical_path(workspace);
    let entries = match fs::read_dir(dir(root, repo)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Blockers {
                ids: Vec::new(),
                _guard: guard,
            });
        }
        Err(error) => return Err(error).context("reading task registry for worktree cleanup"),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let path = entry
            .context("reading task registry entry for worktree cleanup")?
            .path();
        let Some(task) = cleanup_record(&path, repo)? else {
            continue;
        };
        if cache::canonical_path(Path::new(&task.workspace)) == target
            && matches!(task.lifecycle, Lifecycle::Running | Lifecycle::Recovering)
            && ignore != Some(task.id.as_str())
        {
            ids.push(task.id);
        }
    }
    ids.sort();
    ids.dedup();
    Ok(Blockers { ids, _guard: guard })
}

pub(crate) fn load(root: &Path, repo: &str, id: &str) -> Result<Task> {
    let path = path(root, repo, id);
    let bytes = fs::read(&path).with_context(|| format!("no task {id} in this repository"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}
pub fn begin(req: Begin<'_>) -> Result<BeginOutcome> {
    let workspace = cache::canonical_path(req.workspace);
    let repo = project::repo_identity(&workspace);
    let _lifecycle = cache::lifecycle_shared(req.root, &workspace)?;
    // Resolve before taking coordination locks: `crate:` scopes run cargo metadata.
    let resolved_scope = claim::resolve_scopes(&workspace, &req.scope)?;
    let ttl = claim::ttl(&workspace);
    let _workspace_lock = snapshot::workspace_lock(req.root, &workspace)?;
    let _evidence_lock = crate::verify::evidence_lock(req.root)?;
    let scope_snapshot = snapshot::persist(req.root, &repo, &snapshot::capture(&workspace)?)?;
    let _lock = claim::registry_lock(req.root, &repo)?;
    if !workspace.is_dir() || project::repo_identity(&workspace) != repo {
        bail!("workspace changed or disappeared before task publication");
    }
    let conflicts = claim::conflicts_unlocked(
        req.root,
        &repo,
        Some(&workspace),
        ttl,
        &resolved_scope,
        None,
        req.claim_group.as_deref(),
    )?;
    if !conflicts.is_empty() {
        return Ok(BeginOutcome::Conflict {
            requested: req.scope,
            conflicts,
        });
    }
    let now = now_secs();
    let task = Task {
        schema_version: SCHEMA_VERSION,
        id: task_id(),
        branch: git::capture(&workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok(),
        toolchain: project::toolchain(&workspace),
        workspace: workspace.to_string_lossy().into_owned(),
        repo,
        agent: req.agent,
        description: req.description,
        scope: req.scope,
        resolved_scope,
        scope_snapshot: Some(scope_snapshot),
        claim_group: req.claim_group,
        created_at: now,
        last_activity: now,
        lifecycle: Lifecycle::Running,
        commands: Vec::new(),
        reason: None,
        verification: Verification::Unverified,
        verification_reason: None,
        source_sha256: None,
        recovery: None,
    };
    write(req.root, &task)?;
    crate::events::record(
        req.root,
        &task.repo,
        "task.begun",
        serde_json::json!({"task_id": task.id, "agent": task.agent, "scope": task.resolved_scope}),
    );
    drop(_lock);
    task_activity::renew(req.root, &task);
    Ok(BeginOutcome::Begun {
        task: Box::new(task),
    })
}
pub(crate) fn live_claims(root: &Path, repo: &str) -> Result<Vec<claim::Claim>> {
    Ok(records(root, repo)?
        .into_iter()
        .filter(|task| matches!(task.lifecycle, Lifecycle::Running | Lifecycle::Recovering))
        .map(|task| claim::Claim {
            id: task.id,
            agent: task.agent,
            task: task.description,
            scope: task.scope,
            resolved_scope: task.resolved_scope,
            group: task.claim_group,
            branch: task.branch,
            created_at: task.created_at,
        })
        .collect())
}
#[path = "task_activity.rs"]
mod task_activity;
#[path = "task_scope.rs"]
mod task_scope;
#[path = "task_transition.rs"]
mod task_transition;
pub use task_activity::exec;
pub use task_activity::{EXIT_TERMINATED, EXIT_TIMEOUT};
pub(crate) use task_activity::{lane_busy, reconciled, renew};
pub(crate) use task_scope::outside_scope;
pub use task_transition::abandon;
pub(crate) use task_transition::finish_with_verification_locked;

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn supported_legacy_schema_preserves_cleanup_authority() {
        let root = tempdir().unwrap();
        let path = root.path().join("legacy.json");
        fs::write(
            &path,
            r#"{
                "schema_version": 2,
                "id": "legacy",
                "repo": "repo",
                "agent": "agent",
                "description": "legacy task",
                "scope": [],
                "workspace": "/workspace",
                "toolchain": "stable",
                "branch": null,
                "created_at": 1,
                "last_activity": 1,
                "lifecycle": "finished",
                "commands": [],
                "reason": null,
                "verification": "unverified"
            }"#,
        )
        .unwrap();

        let task = cleanup_record(&path, "repo").unwrap().unwrap();
        assert_eq!(task.id, "legacy");
        assert!(task.source_sha256.is_none());
    }
}
