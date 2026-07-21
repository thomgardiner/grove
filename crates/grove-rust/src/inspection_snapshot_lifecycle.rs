use super::*;
use std::ffi::OsStr;

pub fn open(root: &Path, workspace: &Path, capsule_id: &str) -> Result<Capsule> {
    open_with(root, workspace, capsule_id, true)
}

pub fn open_for_cleanup(root: &Path, workspace: &Path, capsule_id: &str) -> Result<Capsule> {
    open_with(root, workspace, capsule_id, false)
}

fn open_with(root: &Path, workspace: &Path, capsule_id: &str, live: bool) -> Result<Capsule> {
    validate_id(capsule_id)?;
    let source = fs::canonicalize(workspace)
        .with_context(|| format!("resolving {}", workspace.display()))?;
    let provisional = Request {
        root,
        workspace: &source,
        task_id: "provisional",
        capsule_id,
        expires_at: u64::MAX,
    };
    let paths = paths(&provisional, false)?;
    let binding: Binding = serde_json::from_slice(&read_lease(&paths.lease)?)
        .with_context(|| format!("parsing {}", paths.lease.display()))?;
    binding.validate()?;
    let request = Request {
        root,
        workspace: &source,
        task_id: &binding.task_id,
        capsule_id,
        expires_at: binding.expires_at,
    };
    if binding != expected_binding(&request, &paths, &binding.source_sha256)? {
        bail!("inspection capsule binding does not match the expected authority")
    }
    if live && binding.expires_at <= now() {
        bail!("inspection capsule lease has expired")
    }
    repository::independent(&source, &paths.workspace)?;
    Ok(Capsule {
        path: paths.workspace,
        lease: paths.lease,
        binding,
    })
}

pub fn digest(workspace: &Path) -> Result<String> {
    Ok(snapshot::capture_read_only(workspace)?.sha256)
}

pub fn list(root: &Path, workspace: &Path) -> Result<Vec<String>> {
    let source = fs::canonicalize(workspace)?;
    let request = Request {
        root,
        workspace: &source,
        task_id: "provisional",
        capsule_id: "provisional",
        expires_at: u64::MAX,
    };
    let namespace = paths(&request, true)?.namespace;
    let mut ids = Vec::new();
    for entry in fs::read_dir(namespace)? {
        let entry = entry?;
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("inspection capsule id is not UTF-8"))?;
        validate_id(&id)?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse(&metadata) {
            bail!("inspection namespace contains an invalid capsule entry")
        }
        ids.push(id);
    }
    ids.sort();
    Ok(ids)
}

pub fn remove(capsule: &Capsule) -> Result<()> {
    let dir = capsule
        .path
        .parent()
        .context("inspection capsule has no authority directory")?;
    if capsule.lease.parent() != Some(dir)
        || dir.file_name() != Some(OsStr::new(&capsule.binding.capsule_id))
    {
        bail!("inspection capsule removal authority does not match its path")
    }
    fs::remove_dir_all(dir)
        .with_context(|| format!("removing inspection capsule {}", dir.display()))
}
