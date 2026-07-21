//! Exact standalone Git snapshots for untrusted inspection processes.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, git, snapshot};

#[path = "inspection_snapshot_files.rs"]
mod files;
#[path = "inspection_snapshot_git.rs"]
mod repository;
#[path = "inspection_snapshot_namespace.rs"]
mod state;

/// Durable inspection-capsule binding schema.
pub const SCHEMA_VERSION: u32 = 1;
const MAX_LEASE_BYTES: u64 = 16 * 1024;

/// Authority record binding one capsule to the captured task state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub schema_version: u32,
    pub task_id: String,
    pub source_sha256: String,
    pub repository_sha256: String,
    pub namespace_sha256: String,
    pub capsule_id: String,
    pub platform: String,
    pub expires_at: u64,
}

/// Inputs for creating one standalone inspection capsule.
pub struct Request<'a> {
    pub root: &'a Path,
    pub workspace: &'a Path,
    pub task_id: &'a str,
    pub capsule_id: &'a str,
    pub expires_at: u64,
}

/// Materialized capsule paths and their validated authority binding.
pub struct Capsule {
    pub path: PathBuf,
    pub lease: PathBuf,
    pub binding: Binding,
}

struct Paths {
    root: PathBuf,
    source: PathBuf,
    namespace: PathBuf,
    dir: PathBuf,
    workspace: PathBuf,
    lease: PathBuf,
}

/// Capture and materialize one exact, standalone Git repository under the workspace lock.
pub fn acquire(request: &Request<'_>) -> Result<Capsule> {
    validate_request(request)?;
    let paths = paths(request, true)?;
    let _lock = snapshot::workspace_lock(&paths.root, &paths.source)?;
    create_private_dir(&paths.dir)
        .with_context(|| format!("creating inspection capsule {}", paths.dir.display()))?;
    match acquire_created(request, &paths) {
        Ok(capsule) => Ok(capsule),
        Err(error) => match fs::remove_dir_all(&paths.dir) {
            Ok(()) => Err(error),
            Err(cleanup) => {
                Err(error.context(format!("also failed to remove partial capsule: {cleanup}")))
            }
        },
    }
}

fn acquire_created(request: &Request<'_>, paths: &Paths) -> Result<Capsule> {
    repository::validate_source(&paths.source, &paths.dir)?;
    files::validate(&paths.source)?;
    let start = snapshot::capture_read_only(&paths.source)?;
    validate_snapshot(&paths.source, &start)?;
    populate(request, paths, &start)
}

fn populate(request: &Request<'_>, paths: &Paths, start: &snapshot::Snapshot) -> Result<Capsule> {
    repository::materialize(&paths.source, &paths.workspace, start)?;
    files::overlay(&paths.source, &paths.workspace, start)?;
    files::validate(&paths.source)?;
    if snapshot::capture_read_only(&paths.source)? != *start {
        bail!("source changed while materializing inspection capsule")
    }
    if snapshot::capture(&paths.workspace)? != *start {
        bail!("inspection capsule does not match the captured source")
    }
    repository::independent(&paths.source, &paths.workspace)?;
    let binding = binding(request, paths, start);
    cache::write_atomic(&paths.lease, &serde_json::to_vec_pretty(&binding)?)?;
    Ok(Capsule {
        path: paths.workspace.clone(),
        lease: paths.lease.clone(),
        binding,
    })
}

/// Load a binding only from the authoritative namespace and match every expected field.
pub fn load(request: &Request<'_>, source_sha256: &str) -> Result<Binding> {
    validate_request(request)?;
    let paths = paths(request, false)?;
    let bytes = read_lease(&paths.lease)?;
    let binding: Binding = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", paths.lease.display()))?;
    binding.validate()?;
    if binding.expires_at <= now() {
        bail!("inspection capsule lease has expired")
    }
    let expected = expected_binding(request, &paths, source_sha256)?;
    if binding != expected {
        bail!("inspection capsule binding does not match the expected authority")
    }
    Ok(binding)
}

fn read_lease(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let before = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !regular(&named) || !same_file(&before, &named) || before.len() > MAX_LEASE_BYTES {
        bail!("inspection capsule binding is not a bounded regular file")
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_LEASE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let renamed = fs::symlink_metadata(path)?;
    if bytes.len() as u64 > MAX_LEASE_BYTES
        || !same_file(&before, &after)
        || !same_file(&after, &renamed)
    {
        bail!("inspection capsule binding changed while it was read")
    }
    Ok(bytes)
}

impl Binding {
    fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!("unsupported inspection capsule schema")
        }
        validate_id(&self.capsule_id)?;
        validate_task(&self.task_id)?;
        if !digest(&self.source_sha256)
            || !digest(&self.repository_sha256)
            || !digest(&self.namespace_sha256)
        {
            bail!("inspection capsule has an invalid source digest")
        }
        if self.platform != platform() || self.expires_at == 0 {
            bail!("inspection capsule binding is incomplete")
        }
        Ok(())
    }
}

fn validate_request(request: &Request<'_>) -> Result<()> {
    validate_id(request.capsule_id)?;
    validate_task(request.task_id)?;
    if request.expires_at <= now() {
        bail!("inspection capsule expiry must be in the future")
    }
    Ok(())
}

fn validate_task(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        bail!("inspection capsule needs a valid task id")
    }
    Ok(())
}

fn lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(lower_hex)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("inspection capsule id must contain only ASCII letters, digits, '-' and '_'")
    }
    Ok(())
}

fn paths(request: &Request<'_>, create: bool) -> Result<Paths> {
    let source = fs::canonicalize(request.workspace)
        .with_context(|| format!("resolving {}", request.workspace.display()))?;
    let top = fs::canonicalize(git::capture(&source, &["rev-parse", "--show-toplevel"])?)
        .context("resolving inspection repository root")?;
    if source != top {
        bail!("inspection source must be the Git repository root")
    }
    let root = state::root(request.root, &source, create)?;
    let inspections = state::child(&root, "inspections", create)?;
    let slug = cache::repo_slug(&source.to_string_lossy());
    let namespace = state::child(&inspections, &slug, create)?;
    if namespace.starts_with(&source) || !namespace.starts_with(&root) {
        bail!("inspection capsules must live outside the source workspace")
    }
    let dir = if create {
        namespace.join(request.capsule_id)
    } else {
        state::child(&namespace, request.capsule_id, false)?
    };
    let workspace = if create {
        dir.join("workspace")
    } else {
        state::child(&dir, "workspace", false)?
    };
    Ok(Paths {
        root,
        source,
        namespace,
        workspace,
        lease: dir.join("lease.json"),
        dir,
    })
}

fn validate_snapshot(source: &Path, start: &snapshot::Snapshot) -> Result<()> {
    start
        .head()
        .context("inspection capsules reject an unborn repository")?;
    if start
        .entries
        .iter()
        .any(|entry| entry.mode == Some(0o160000))
    {
        bail!("inspection capsules reject submodule state")
    }
    snapshot::validate_frozen_links(source, start)?;
    files::validate_links(source, start)?;
    files::validate(source)?;
    Ok(())
}

fn binding(request: &Request<'_>, paths: &Paths, start: &snapshot::Snapshot) -> Binding {
    Binding {
        schema_version: SCHEMA_VERSION,
        task_id: request.task_id.to_string(),
        source_sha256: start.sha256.clone(),
        repository_sha256: path_digest(&paths.source),
        namespace_sha256: path_digest(&paths.namespace),
        capsule_id: request.capsule_id.to_string(),
        platform: platform(),
        expires_at: request.expires_at,
    }
}

fn expected_binding(request: &Request<'_>, paths: &Paths, source: &str) -> Result<Binding> {
    if !digest(source) {
        bail!("expected inspection source digest is invalid")
    }
    Ok(Binding {
        schema_version: SCHEMA_VERSION,
        task_id: request.task_id.to_string(),
        source_sha256: source.to_string(),
        repository_sha256: path_digest(&paths.source),
        namespace_sha256: path_digest(&paths.namespace),
        capsule_id: request.capsule_id.to_string(),
        platform: platform(),
        expires_at: request.expires_at,
    })
}

fn path_digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()))
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.file_attributes() == right.file_attributes()
        && left.creation_time() == right.creation_time()
        && left.last_write_time() == right.last_write_time()
        && left.file_size() == right.file_size()
}

fn regular(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink() && !reparse(metadata)
}

#[cfg(windows)]
fn reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn reparse(_metadata: &fs::Metadata) -> bool {
    false
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "inspection_snapshot_tests.rs"]
mod tests;
