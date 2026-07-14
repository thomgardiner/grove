//! Safe export of a finished artifact from a tagged build lane.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::api::Grove;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Details of one exported artifact.
#[derive(Debug, serde::Serialize)]
pub struct Export {
    /// Canonical source path inside the held lane.
    pub source: String,
    /// Destination path requested by the caller.
    pub destination: String,
    /// SHA-256 digest of the exact bytes copied to the destination.
    pub sha256: String,
    /// Whether the caller established matching successful verification evidence.
    pub verified: bool,
}

/// Copy `source` from `tag`'s held lane to `destination` atomically.
///
/// The lane lease keeps cache garbage collection from removing its source while the
/// copy is in progress. `source` must be a relative regular-file path which resolves
/// below that lane; symlinks are allowed only when their final target remains there.
/// Existing destinations are left untouched rather than replaced.
pub fn export(
    grove: &Grove,
    tag: &str,
    source: &Path,
    destination: &Path,
    verified: bool,
) -> Result<Export> {
    let lane = grove.tagged_lane(tag)?;
    let source = resolve(&lane.dir, source)?;
    let sha256 = copy(&source, destination)?;

    let export = Export {
        source: source.display().to_string(),
        destination: destination.display().to_string(),
        sha256,
        verified,
    };
    drop(lane);
    Ok(export)
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

fn copy(source: &Path, destination: &Path) -> Result<String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).context("creating artifact destination directory")?;
    if destination.exists() {
        bail!("artifact destination already exists; refusing to replace it");
    }

    let temp = temporary(parent, destination)?;
    let result = copy_to(source, &temp, destination);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
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

fn copy_to(source: &Path, temp: &Path, destination: &Path) -> Result<String> {
    let mut input = File::open(source).context("opening artifact source")?;
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
    drop(output);
    // A hard link creates the destination atomically only when it does not already
    // exist, unlike POSIX rename which may replace a concurrently-created file.
    fs::hard_link(temp, destination).context("publishing artifact")?;
    let _ = fs::remove_file(temp);
    Ok(format!("{:x}", hash.finalize()))
}
