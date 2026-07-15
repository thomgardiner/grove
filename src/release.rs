//! Frozen, signed release bundles built from one verified workspace snapshot.

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::SigningKey;
use fs2::FileExt;
use serde::Serialize;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};

use crate::api::Grove;
use crate::{cache, project, snapshot, task, verify};

#[path = "release_bundle.rs"]
mod bundle;
#[path = "release_snapshot.rs"]
mod frozen;

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
    receipts: Vec<ReleaseReceipt>,
    verification: ReleaseVerification,
    artifacts: Vec<Artifact>,
    signer_public_key: String,
    signer_key_id: String,
}

#[derive(Serialize)]
struct ReleaseVerification {
    passed: bool,
    receipt_count: usize,
}

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

struct DestinationLock {
    file: File,
}

impl Drop for DestinationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Snapshot a task's exact content into an isolated worktree, verify it in a one-use
/// lane, and publish a signed bundle without executing release commands in the source.
pub fn freeze(
    root: &Path,
    workspace: &Path,
    task_id: &str,
    profile: &str,
    artifacts: &[String],
    out: &Path,
) -> Result<Report> {
    if artifacts.is_empty() {
        bail!("release freeze requires at least one --artifact")
    }
    let workspace = cache::canonical_path(workspace);
    let out = release_destination(&workspace, out)?;
    let _destination_lock = destination_lock(root, &out)?;
    let repo = project::repo_identity(&workspace);
    let task = task::load(root, &repo, task_id)?;
    if task.workspace != workspace.to_string_lossy() {
        bail!("task {task_id} belongs to a different workspace")
    }
    let _workspace_lock = snapshot::workspace_lock(root, &workspace)?;
    let outside_scope = task::outside_scope(root, &repo, task_id)?;
    if !outside_scope.is_empty() {
        bail!(
            "task {task_id} wrote outside its declared scope: {}",
            outside_scope.join(", ")
        )
    }
    let start = snapshot::capture(&workspace)?;
    let start_ref = snapshot::persist(root, &repo, &start)?;
    let signer = signing_key()?;
    let frozen = frozen::materialize(root, &workspace, &start)?;
    require_unchanged(&workspace, &start)?;
    cache::maintain(root, || {
        let lane_tag = format!(
            "release-freeze-{}",
            cache::repo_slug(&frozen.path().to_string_lossy())
        );
        let lane = Grove::with_root(root.to_path_buf(), frozen.path()).tagged_lane(&lane_tag)?;
        if lane.target_dir.exists() {
            let target = lane.target_dir.clone();
            cache::discard(lane);
            bail!("frozen release lane was not empty: {}", target.display())
        }
        let result = (|| {
            let run = verify::run_locked_in_lane(root, frozen.path(), profile, None, &lane)?;
            if !run.passed {
                bail!("release verification profile {profile:?} failed")
            }
            require_unchanged(&workspace, &start)?;
            require_unchanged(frozen.path(), &start)?;
            let stage = stage_dir(&out)?;
            let result = bundle::stage_bundle(
                &workspace,
                frozen.path(),
                &repo,
                task_id,
                profile,
                artifacts,
                &start,
                start_ref.clone(),
                run,
                &signer,
                &lane,
                &stage,
            );
            if result.is_err() {
                let _ = fs::remove_dir_all(&stage);
            }
            result
        })();
        cache::discard(lane);
        result
    })
}

fn signing_key() -> Result<SigningKey> {
    let encoded = std::env::var("GROVE_RELEASE_SIGNING_KEY")
        .context("GROVE_RELEASE_SIGNING_KEY is required for release freeze")?;
    let bytes = STANDARD
        .decode(encoded.trim())
        .context("GROVE_RELEASE_SIGNING_KEY must be base64")?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .context("GROVE_RELEASE_SIGNING_KEY must decode to a 32-byte Ed25519 seed")?;
    Ok(SigningKey::from_bytes(&seed))
}

fn require_unchanged(workspace: &Path, start: &snapshot::Snapshot) -> Result<()> {
    if snapshot::capture(workspace)?.sha256 != start.sha256 {
        bail!("workspace content drifted during frozen release")
    }
    Ok(())
}

fn release_destination(workspace: &Path, out: &Path) -> Result<PathBuf> {
    let out = if out.is_absolute() {
        normalize(out)
    } else {
        normalize(
            &std::env::current_dir()
                .context("resolving release destination")?
                .join(out),
        )
    };
    if out.starts_with(workspace) {
        bail!("release bundle destination must be outside the workspace")
    }
    let parent = out
        .parent()
        .context("release bundle needs a parent directory")?;
    let name = out
        .file_name()
        .context("release bundle needs a directory name")?;
    let existing = parent
        .ancestors()
        .find(|candidate| candidate.exists())
        .context("release bundle destination has no existing ancestor")?;
    if fs::canonicalize(existing)
        .context("resolving release destination ancestor")?
        .starts_with(workspace)
    {
        bail!("release bundle destination must be outside the workspace")
    }
    fs::create_dir_all(parent).context("creating release destination parent")?;
    let parent = fs::canonicalize(parent).context("resolving release destination parent")?;
    if parent.starts_with(workspace) {
        bail!("release bundle destination must be outside the workspace")
    }
    // Keep the physical parent: later changes to a caller-supplied symlink cannot
    // redirect the destination after this outside-workspace check.
    Ok(parent.join(name))
}

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

fn stage_dir(out: &Path) -> Result<PathBuf> {
    match fs::create_dir(out) {
        Ok(()) => {
            let parent = out
                .parent()
                .context("release bundle needs a parent directory")?;
            let claimed = fs::canonicalize(out).context("resolving claimed release destination")?;
            if claimed.parent() != Some(parent) {
                bail!("release bundle destination parent changed while claiming it")
            }
            Ok(out.to_path_buf())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("release bundle destination already exists")
        }
        Err(error) => Err(error).context("claiming release bundle destination"),
    }
}

#[cfg(test)]
mod tests {
    use super::{release_destination, stage_dir};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn stage_claim_preserves_existing_destination() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("bundle");
        fs::create_dir(&out).unwrap();
        let sentinel = out.join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();

        assert!(stage_dir(&out).is_err());
        assert_eq!(fs::read(sentinel).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn destination_keeps_resolved_parent_after_symlink_retarget() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let link = dir.path().join("out");
        symlink(&first, &link).unwrap();

        let out = release_destination(&workspace, &link.join("bundle")).unwrap();
        fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();

        stage_dir(&out).unwrap();
        assert!(out.exists());
        assert!(!second.join("bundle").exists());
    }
}
