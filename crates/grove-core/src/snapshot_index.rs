use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use super::super::{Entry, Kind};

pub(super) fn gitlinks(workspace: &Path) -> Result<BTreeMap<String, Entry>> {
    let output = Command::new("git")
        .args(["ls-files", "--stage", "-z"])
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("reading gitlinks from the Git index")?;
    if !output.status.success() {
        bail!(
            "reading gitlinks from the Git index failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let mut entries = BTreeMap::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record).context("Git index path is not UTF-8")?;
        let (metadata, path) = record
            .split_once('\t')
            .context("malformed Git index entry")?;
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().context("Git index entry has no mode")?;
        if mode != "160000" {
            continue;
        }
        let oid = fields.next().context("Git index entry has no object ID")?;
        if fields.next() != Some("0") || fields.next().is_some() {
            bail!("Git index returned a non-zero stage for a gitlink")
        }
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("Git index returned an invalid gitlink object ID")
        }
        entries.insert(
            path.into(),
            Entry {
                path: path.into(),
                tracked: true,
                // Mode 160000 keeps gitlinks distinct while retaining the
                // snapshot's filesystem-oriented entry kinds.
                kind: Kind::File,
                sha256: Some(hash(oid.as_bytes())?),
                mode: Some(0o160000),
            },
        );
    }
    Ok(entries)
}

pub(super) fn missing(workspace: &Path, path: &str) -> Result<Option<Entry>> {
    let literal = format!(":(literal){path}");
    let output = Command::new("git")
        .args(["ls-files", "-t", "--stage", "-z", "--"])
        .arg(literal)
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("reading missing path from the Git index")?;
    if !output.status.success() {
        bail!(
            "reading missing path from the Git index failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let record = output
        .stdout
        .strip_suffix(b"\0")
        .context("Git index omitted a tracked path")?;
    if !record.starts_with(b"S ") {
        return Ok(None);
    }
    let record = std::str::from_utf8(&record[2..]).context("Git index path is not UTF-8")?;
    let (metadata, actual) = record
        .split_once('\t')
        .context("malformed Git index entry")?;
    if actual != path {
        bail!("Git index returned the wrong tracked path")
    }
    let mut fields = metadata.split_whitespace();
    let mode = fields.next().context("Git index entry has no mode")?;
    let oid = fields.next().context("Git index entry has no object ID")?;
    if fields.next() != Some("0") || fields.next().is_some() {
        bail!("Git index returned a non-zero stage for a sparse path")
    }
    let (kind, unix_mode) = match mode {
        "100644" => (Kind::File, 0o100644),
        "100755" => (Kind::File, 0o100755),
        "120000" => (Kind::Symlink, 0o120000),
        _ => bail!("verification snapshot refuses Git index mode {mode}"),
    };
    Ok(Some(Entry {
        path: path.into(),
        tracked: true,
        kind,
        sha256: Some(object_hash(workspace, path, oid)?),
        mode: Some(if cfg!(unix) { unix_mode } else { 0 }),
    }))
}

fn object_hash(workspace: &Path, path: &str, oid: &str) -> Result<String> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Git index returned an invalid object ID")
    }
    let filter_path = format!("--path={path}");
    let mut child = Command::new("git")
        .args(["cat-file", "--filters"])
        .arg(filter_path)
        .arg(oid)
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("reading sparse Git blob")?;
    let stdout = child
        .stdout
        .take()
        .context("Git blob pipe is unavailable")?;
    let hash = match hash(stdout) {
        Ok(hash) => hash,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("hashing sparse Git blob");
        }
    };
    if !child
        .wait()
        .context("waiting for sparse Git blob")?
        .success()
    {
        bail!("reading sparse Git blob failed")
    }
    Ok(hash)
}

fn hash(mut input: impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buf)?;
        if count == 0 {
            break;
        }
        hash.update(&buf[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
