//! Durable, language-neutral verification evidence.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::snapshot;

pub const RECEIPT_SCHEMA_VERSION: u32 = 5;
static RECEIPT_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub head: Option<String>,
    pub changed_paths: Vec<String>,
    pub branch: Option<String>,
    pub workspace: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LaneIdentity {
    pub tag: String,
    pub path: String,
    #[serde(default)]
    pub policy_sha256: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Evidence {
    #[serde(flatten)]
    pub checkout: Checkout,
    pub input: snapshot::Ref,
    pub output: snapshot::Ref,
    #[serde(default)]
    pub portable: Option<serde_json::Value>,
}

/// A bounded, machine-readable outcome for one verification command.
#[derive(Serialize, Deserialize, Clone)]
pub struct Receipt {
    pub schema_version: u32,
    pub repository: String,
    pub task_id: Option<String>,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub profile: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub profile_sha256: String,
    /// Content digests of profile-declared trusted inputs at verification time.
    #[serde(default)]
    pub input_digests: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub command_index: usize,
    pub required: bool,
    #[serde(flatten)]
    pub evidence: Option<Evidence>,
    pub lane: LaneIdentity,
    pub argv: Vec<String>,
    pub started_at: u64,
    pub ended_at: u64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub interrupted: bool,
    pub test_count: Option<u64>,
    pub passed: bool,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// A durable completion record binding an ordered profile run's receipts.
#[derive(Serialize, Deserialize, Clone)]
pub struct Run {
    pub schema_version: u32,
    pub repository: String,
    pub task_id: Option<String>,
    pub profile: String,
    pub run_id: String,
    pub profile_sha256: String,
    pub command_count: usize,
    pub receipt_count: usize,
    pub passed: bool,
    pub completed_at_nanos: u128,
}

pub struct StoredReceipt {
    pub slug: String,
    pub receipt: Receipt,
}

pub struct StoredRun {
    pub slug: String,
    pub run: Run,
}

pub fn write_receipt(root: &Path, repo: &str, receipt: &Receipt) -> Result<()> {
    let seq = RECEIPT_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = root
        .join("receipts")
        .join(crate::repo_slug(repo))
        .join(format!(
            "{:x}-{:x}-{seq:x}.json",
            now_secs(),
            std::process::id()
        ));
    crate::write_atomic(&path, &serde_json::to_vec_pretty(receipt)?)
}

pub fn receipts(root: &Path, repo: &str) -> Result<Vec<Receipt>> {
    records(root.join("receipts").join(crate::repo_slug(repo)))
}

pub fn complete_run(root: &Path, repo: &str, run: &Run) -> Result<()> {
    if run.run_id.is_empty()
        || !run
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        bail!("invalid verification run id")
    }
    let path = root
        .join("verification-runs")
        .join(crate::repo_slug(repo))
        .join(format!("{}.json", run.run_id));
    crate::write_atomic(&path, &serde_json::to_vec_pretty(run)?)
}

pub fn runs(root: &Path, repo: &str) -> Result<Vec<Run>> {
    records(root.join("verification-runs").join(crate::repo_slug(repo)))
}

pub fn all_receipts(root: &Path) -> Vec<StoredReceipt> {
    all(root, "receipts", |bytes| serde_json::from_slice(bytes))
        .into_iter()
        .map(|(slug, receipt)| StoredReceipt { slug, receipt })
        .collect()
}

pub fn all_runs(root: &Path) -> Vec<StoredRun> {
    all(root, "verification-runs", |bytes| {
        serde_json::from_slice(bytes)
    })
    .into_iter()
    .map(|(slug, run)| StoredRun { slug, run })
    .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn records<T: serde::de::DeserializeOwned>(dir: PathBuf) -> Result<Vec<T>> {
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| {
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        })
        .collect()
}

fn all<T>(
    root: &Path,
    kind: &str,
    parse: impl Fn(&[u8]) -> serde_json::Result<T>,
) -> Vec<(String, T)> {
    let Ok(buckets) = fs::read_dir(root.join(kind)) else {
        return Vec::new();
    };
    buckets
        .filter_map(|bucket| bucket.ok())
        .filter(|bucket| bucket.file_type().is_ok_and(|kind| kind.is_dir()))
        .flat_map(|bucket| {
            let slug = bucket.file_name().to_string_lossy().into_owned();
            let Ok(records) = fs::read_dir(bucket.path()) else {
                return Vec::new().into_iter();
            };
            records
                .filter_map(|record| record.ok())
                .map(|record| record.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "json")
                })
                .filter_map(|path| {
                    parse(&fs::read(path).ok()?)
                        .ok()
                        .map(|record| (slug.clone(), record))
                })
                .collect::<Vec<_>>()
                .into_iter()
        })
        .collect()
}
