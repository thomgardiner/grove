//! Safe export of a finished artifact from a tagged build lane.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{api::Grove, cache};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Internal authority; callers may only use the public explicit-override path.
#[derive(Debug, Clone)]
enum Authorization {
    Verified,
    Overridden { reason: String },
}

/// Details of one exported artifact.
#[derive(Debug, Clone, Serialize)]
pub struct Export {
    /// Canonical source path inside the held lane.
    pub source: String,
    /// Destination path requested by the caller.
    pub destination: String,
    /// SHA-256 digest of the exact bytes copied to the destination.
    pub sha256: String,
    /// Whether the caller established matching successful verification evidence.
    pub verified: bool,
    /// The audited escape-hatch reason when this export was explicitly unverified.
    pub override_reason: Option<String>,
}

#[derive(Serialize)]
struct Audit<'a> {
    schema_version: u32,
    created_at: u128,
    repository: &'a str,
    workspace: String,
    published: bool,
    #[serde(flatten)]
    export: &'a Export,
}

/// Copy `source` from `tag`'s held lane to `destination` atomically.
///
/// The lane lease keeps cache garbage collection from removing its source while the
/// copy is in progress. `source` must be a relative regular-file path which resolves
/// below that lane; symlinks are allowed only when their final target remains there.
/// Existing destinations are left untouched rather than replaced.
/// Export with an explicit, durable unverified exception. Successful verification must
/// flow through [`crate::verify::export`], which constructs internal authority only
/// after checking the named task's current receipts.
pub fn export_override(
    grove: &Grove,
    tag: &str,
    source: &Path,
    destination: &Path,
    reason: String,
) -> Result<Export> {
    if reason.trim().is_empty() {
        bail!("--allow-unverified requires a nonempty reason")
    }
    export(
        grove,
        tag,
        source,
        destination,
        Authorization::Overridden { reason },
    )
}

pub(crate) fn export_verified(
    grove: &Grove,
    tag: &str,
    source: &Path,
    destination: &Path,
) -> Result<Export> {
    export(grove, tag, source, destination, Authorization::Verified)
}

fn export(
    grove: &Grove,
    tag: &str,
    source: &Path,
    destination: &Path,
    authorization: Authorization,
) -> Result<Export> {
    let (verified, override_reason) = match authorization {
        Authorization::Verified => (true, None),
        Authorization::Overridden { reason } => (false, Some(reason)),
    };
    let lane = grove.tagged_lane(tag)?;
    let source = resolve(&lane.dir, source)?;
    let (staged, sha256) = stage(&source, destination)?;

    let export = Export {
        source: source.display().to_string(),
        destination: destination.display().to_string(),
        sha256,
        verified,
        override_reason,
    };
    let audit_record = audit_path(grove);
    if let Err(error) = audit(grove, &audit_record, &export, false) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    if let Err(error) = publish(&staged, destination) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    if let Err(error) = audit(grove, &audit_record, &export, true) {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    drop(lane);
    Ok(export)
}

fn audit_path(grove: &Grove) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    grove
        .root()
        .join("exports")
        .join(cache::repo_slug(grove.repo()))
        .join(format!("{now:x}-{:x}.json", std::process::id()))
}

fn audit(grove: &Grove, path: &Path, export: &Export, published: bool) -> Result<()> {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    cache::write_atomic(
        path,
        &serde_json::to_vec_pretty(&Audit {
            schema_version: 1,
            created_at,
            repository: grove.repo(),
            workspace: grove.workspace().to_string_lossy().into_owned(),
            published,
            export,
        })?,
    )
}

fn resolve(lane: &Path, source: &Path) -> Result<PathBuf> {
    if source.is_absolute() || source.components().any(|part| part == Component::ParentDir) {
        bail!("artifact source must be a relative path beneath its lane");
    }

    let lane = fs::canonicalize(lane).context("resolving artifact lane")?;
    let source = fs::canonicalize(lane.join(source)).context("resolving artifact source")?;
    if !source.starts_with(&lane) {
        bail!("artifact source escapes its lane");
    }
    if !source.is_file() {
        bail!("artifact source is not a regular file");
    }
    Ok(source)
}

fn stage(source: &Path, destination: &Path) -> Result<(PathBuf, String)> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).context("creating artifact destination directory")?;
    if destination.exists() {
        bail!("artifact destination already exists; refusing to replace it");
    }

    let temp = temporary(parent, destination)?;
    let result = copy_to(source, &temp);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map(|sha256| (temp, sha256))
}

fn temporary(parent: &Path, destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    for _ in 0..64 {
        let temp = parent.join(format!(
            ".{name}.grove-export-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if !temp.exists() {
            return Ok(temp);
        }
    }
    bail!("could not allocate a unique artifact staging path")
}

fn copy_to(source: &Path, temp: &Path) -> Result<String> {
    let mut input = File::open(source).context("opening artifact source")?;
    let permissions = input
        .metadata()
        .context("reading artifact source metadata")?
        .permissions();
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .context("creating artifact staging file")?;
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];

    loop {
        let count = input.read(&mut buf).context("reading artifact source")?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buf[..count])
            .context("writing artifact staging file")?;
        hash.update(&buf[..count]);
    }
    output.sync_all().context("syncing artifact staging file")?;
    // An exported binary must stay a binary: carry the source mode over
    // instead of leaving the staging file's default 0644.
    output
        .set_permissions(permissions)
        .context("preserving artifact source mode")?;
    drop(output);
    Ok(format!("{:x}", hash.finalize()))
}

fn publish(temp: &Path, destination: &Path) -> Result<()> {
    // A hard link creates the destination atomically only when it does not already
    // exist, unlike POSIX rename which may replace a concurrently-created file.
    fs::hard_link(temp, destination).context("publishing artifact")?;
    let _ = fs::remove_file(temp);
    Ok(())
}
