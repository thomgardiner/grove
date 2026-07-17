//! Low-latency best-effort JSONL signal for orchestrators. Claims, tasks, verifications,
//! and reaps attempt to append to `events/<repo>.jsonl` under the cache root, but
//! rotation or write failure can create gaps. Consumers reconcile durable task, claim,
//! lease, and receipt state instead of treating this file as a replayable queue.
//!
//! Recording is synchronous best effort; errors are swallowed so observation failure
//! never breaks the operation it follows.

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

/// Attempt to record one event. Errors are swallowed: observation must not break work.
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
