//! Append-only JSONL event log for orchestrators: claims, tasks, verifications, and
//! reaps land in `events/<repo>.jsonl` under the cache root, so a manager catches up
//! on fleet history with one file read instead of re-deriving it from registry state.
//!
//! Best-effort by design: recording must never fail or slow the operation it records.
//! Each line is one `O_APPEND` write, so concurrent writers do not interleave.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Rotate at this size; one previous generation is kept as `<repo>.jsonl.1`.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Where a repository's event log lives under the cache root.
pub fn path(root: &Path, repo: &str) -> PathBuf {
    root.join("events")
        .join(format!("{}.jsonl", crate::cache::repo_slug(repo)))
}

/// Record one event. Errors are deliberately swallowed: the log observes operations,
/// it must never break them.
pub fn record(root: &Path, repo: &str, event: &str, fields: Value) {
    let _ = try_record(root, repo, event, fields);
}

fn try_record(root: &Path, repo: &str, event: &str, fields: Value) -> std::io::Result<()> {
    let path = path(root, repo);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(&path)
        .map(|meta| meta.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
    }
    let mut line = serde_json::Map::new();
    line.insert("ts".into(), Value::from(now_secs()));
    line.insert("event".into(), Value::from(event));
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut bytes = serde_json::to_vec(&Value::Object(line)).unwrap_or_default();
    bytes.push(b'\n');
    file.write_all(&bytes)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
