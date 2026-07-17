//! Frozen release bundles built from one verified workspace snapshot.

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};
#[cfg(unix)]
use fs2::FileExt;
use serde::Serialize;
#[cfg(unix)]
use std::fs::{self, File};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(unix)]
use std::path::{Component, PathBuf};

#[cfg(unix)]
use crate::api::Grove;
#[cfg(unix)]
use crate::{cache, project, task, verify, worktree};
use crate::{config, snapshot};

#[cfg(unix)]
#[path = "release_bundle.rs"]
mod bundle;
#[cfg(unix)]
#[path = "release_publish_cleanup.rs"]
mod cleanup;
#[cfg(unix)]
#[path = "release_directory.rs"]
mod directory;
#[cfg(unix)]
#[path = "release_snapshot.rs"]
mod frozen;
#[cfg(unix)]
#[path = "release_publish.rs"]
mod publish;

#[derive(Serialize)]
pub struct Report {
    pub bundle: String,
    pub manifest_sha256: String,
    pub snapshot: snapshot::Ref,
    pub artifacts: Vec<Artifact>,
}

#[derive(Serialize, Clone)]
pub struct Artifact {
    pub source: String,
    pub path: String,
    pub sha256: String,
    pub mode: u32,
}

#[cfg(unix)]
#[derive(Serialize)]
struct Manifest {
    schema_version: u32,
    grove_version: &'static str,
    repository_id: String,
    task_id: String,
    toolchain: String,
    profile: String,
    profile_sha256: String,
    commands: Vec<Vec<String>>,
    snapshot: snapshot::Ref,
    snapshot_manifest: snapshot::Snapshot,
    receipts: Vec<ReleaseReceipt>,
    verification: ReleaseVerification,
    artifacts: Vec<Artifact>,
}

#[cfg(unix)]
#[derive(Serialize)]
struct ReleaseVerification {
    passed: bool,
    receipt_count: usize,
}

#[cfg(unix)]
/// Public bundles retain auditable command evidence without diagnostic tails or
/// machine-local repository, workspace, and lane paths.
#[derive(Serialize)]
struct ReleaseReceipt {
    profile: String,
    run_id: String,
    profile_sha256: String,
    command_index: usize,
    required: bool,
    checkout_head: Option<String>,
    changed_paths: Vec<String>,
    input: snapshot::Ref,
    output: snapshot::Ref,
    lane_tag: String,
    argv: Vec<String>,
    started_at: u64,
    ended_at: u64,
    duration_ms: u64,
    exit_code: Option<i32>,
    interrupted: bool,
    test_count: Option<u64>,
    passed: bool,
}

#[cfg(unix)]
struct DestinationLock {
    file: File,
}

#[cfg(unix)]
struct Destination {
    visible: PathBuf,
    visible_parent: PathBuf,
    resolved: PathBuf,
    #[cfg(unix)]
    parent: File,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

#[cfg(unix)]
impl Destination {
    fn visible(&self) -> &Path {
        &self.visible
    }

    fn resolved(&self) -> &Path {
        &self.resolved
    }

    fn matches_parent(&self) -> Result<()> {
        let parent = self.resolved.parent().context("missing release parent")?;
        if fs::canonicalize(&self.visible_parent)? != parent {
            bail!("release bundle destination parent changed after it was resolved")
        }
        #[cfg(unix)]
        {
            let metadata = fs::symlink_metadata(parent)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.dev() != self.dev
                || metadata.ino() != self.ino
            {
                bail!("release bundle destination parent changed after it was resolved")
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn parent_file(&self) -> &File {
        &self.parent
    }
}

#[cfg(unix)]
impl Drop for DestinationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Snapshot a task's exact content into an isolated worktree, verify it in a one-use
/// lane, and publish the bundle without executing release commands in the source.
pub fn freeze(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    task_id: &str,
    profile: &str,
    artifacts: &[String],
    out: &Path,
) -> Result<Report> {
    #[cfg(unix)]
    {
        freeze_unix(root, workspace, config, task_id, profile, artifacts, out)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, workspace, config, task_id, profile, artifacts, out);
        bail!("secure frozen release is not supported on this platform")
    }
}

#[cfg(unix)]
fn freeze_unix(
    root: &Path,
    workspace: &Path,
    config: &config::Config,
    task_id: &str,
    profile: &str,
    artifacts: &[String],
    out: &Path,
) -> Result<Report> {
    if artifacts.is_empty() {
        bail!("release freeze requires at least one --artifact")
    }
    let workspace = cache::canonical_path(workspace);
    let repo = project::repo_identity(&workspace);
    let task = task::load(root, &repo, task_id)?;
    if task.workspace != workspace.to_string_lossy() {
        bail!("task {task_id} belongs to a different workspace")
    }
    worktree::full(root, &workspace)?;
    let task = task::load(root, &repo, task_id)?;
    if task.workspace != workspace.to_string_lossy() {
        bail!("task {task_id} changed workspaces during full conversion")
    }
    let destination = release_destination(&workspace, out)?;
    let _destination_lock = destination_lock(root, destination.resolved())?;
    let mut publication = publish::Publication::prepare(root, &workspace, destination)?;
    let _workspace_lock = snapshot::workspace_lock(root, &workspace)?;
    let outside_scope = task::outside_scope(root, &repo, task_id)?;
    if !outside_scope.is_empty() {
        bail!(
            "task {task_id} wrote outside its declared scope: {}",
            outside_scope.join(", ")
        )
    }
    let _evidence_lock = verify::evidence_lock(root)?;
    let start = snapshot::capture(&workspace)?;
    let start_ref = snapshot::persist(root, &repo, &start)?;
    let mut frozen = frozen::materialize(root, &workspace, &start)?;
    require_unchanged(&workspace, &start)?;
    let grove = Grove::bind(
        root.to_path_buf(),
        frozen.path().to_path_buf(),
        config.clone(),
    );
    let mut report = grove.maintain(|| {
        let lane_tag = format!(
            "release-freeze-{}",
            cache::repo_slug(&frozen.path().to_string_lossy())
        );
        let lane = grove.tagged_lane(&lane_tag)?;
        if lane.target_dir.exists() {
            let target = lane.target_dir.clone();
            cache::discard(lane);
            bail!("frozen release lane was not empty: {}", target.display())
        }
        let result = (|| {
            let run = verify::run_locked_in_lane_with_lock(
                root,
                frozen.path(),
                config,
                profile,
                None,
                &lane_tag,
                &lane,
            )?;
            if !run.passed {
                bail!("release verification profile {profile:?} failed")
            }
            require_unchanged(&workspace, &start)?;
            require_unchanged(frozen.path(), &start)?;
            bundle::stage_bundle(
                &workspace,
                frozen.path(),
                &repo,
                task_id,
                profile,
                artifacts,
                &start,
                start_ref.clone(),
                run,
                &lane,
                publication.stage_file()?,
            )
        })();
        cache::discard(lane);
        result
    })?;
    frozen.cleanup()?;
    publication.publish()?;
    report.bundle = publication.output().display().to_string();
    Ok(report)
}

#[cfg(unix)]
fn require_unchanged(workspace: &Path, start: &snapshot::Snapshot) -> Result<()> {
    if snapshot::capture(workspace)?.sha256 != start.sha256 {
        bail!("workspace content drifted during frozen release")
    }
    Ok(())
}

#[cfg(unix)]
fn release_destination(workspace: &Path, out: &Path) -> Result<Destination> {
    let visible = if out.is_absolute() {
        normalize(out)
    } else {
        normalize(
            &std::env::current_dir()
                .context("resolving release destination")?
                .join(out),
        )
    };
    if visible.starts_with(workspace) {
        bail!("release bundle destination must be outside the workspace")
    }
    let visible_parent = visible
        .parent()
        .context("release bundle needs a parent directory")?
        .to_path_buf();
    let name = visible
        .file_name()
        .context("release bundle needs a directory name")?
        .to_os_string();
    let existing = visible_parent
        .ancestors()
        .find(|candidate| candidate.exists())
        .context("release bundle destination has no existing ancestor")?;
    if fs::canonicalize(existing)
        .context("resolving release destination ancestor")?
        .starts_with(workspace)
    {
        bail!("release bundle destination must be outside the workspace")
    }
    fs::create_dir_all(&visible_parent).context("creating release destination parent")?;
    let resolved_parent =
        fs::canonicalize(&visible_parent).context("resolving release destination parent")?;
    if resolved_parent.starts_with(workspace) {
        bail!("release bundle destination must be outside the workspace")
    }
    #[cfg(unix)]
    let (parent, dev, ino) = pin_destination_parent(&resolved_parent)?;
    Ok(Destination {
        visible,
        visible_parent,
        resolved: resolved_parent.join(name),
        #[cfg(unix)]
        parent,
        #[cfg(unix)]
        dev,
        #[cfg(unix)]
        ino,
    })
}

#[cfg(unix)]
fn pin_destination_parent(path: &Path) -> Result<(File, u64, u64)> {
    let expected = fs::symlink_metadata(path)?;
    if !expected.is_dir() || expected.file_type().is_symlink() {
        bail!("release bundle destination parent is not a real directory")
    }
    let file = File::open(path)?;
    let actual = file.metadata()?;
    if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        bail!("release bundle destination parent changed while opening it")
    }
    Ok((file, actual.dev(), actual.ino()))
}

#[cfg(unix)]
fn destination_lock(root: &Path, out: &Path) -> Result<DestinationLock> {
    let path = root.join("locks").join(format!(
        "release-destination-{}.lock",
        cache::repo_slug(&out.to_string_lossy())
    ));
    fs::create_dir_all(path.parent().context("release lock has no parent")?)?;
    let file = File::create(&path).with_context(|| format!("opening {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(DestinationLock { file })
}

#[cfg(unix)]
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
