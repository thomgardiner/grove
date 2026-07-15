//! Cross-bucket receipt scans used only by portable evidence lookup.

use std::fs;
use std::path::Path;

use super::{Receipt, Run};

/// Receipt records from every local repository bucket. A portable query must inspect
/// these rather than the current checkout's local git-common-dir bucket.
pub(crate) fn all_receipts(root: &Path) -> Vec<StoredReceipt> {
    all(root, "receipts", |bytes| serde_json::from_slice(bytes))
        .into_iter()
        .map(|(slug, receipt)| StoredReceipt { slug, receipt })
        .collect()
}

/// Completion records from every local repository bucket. Invalid JSON is excluded:
/// it cannot establish portable evidence, while retention preserves its whole bucket.
pub(crate) fn all_runs(root: &Path) -> Vec<StoredRun> {
    all(root, "verification-runs", |bytes| {
        serde_json::from_slice(bytes)
    })
    .into_iter()
    .map(|(slug, run)| StoredRun { slug, run })
    .collect()
}

pub(crate) struct StoredReceipt {
    pub(crate) slug: String,
    pub(crate) receipt: Receipt,
}

pub(crate) struct StoredRun {
    pub(crate) slug: String,
    pub(crate) run: Run,
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
