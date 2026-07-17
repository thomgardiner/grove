//! Language-neutral coordination primitives used by Grove adapters.

pub mod claim;
pub mod events;
pub mod git;
pub mod recovery;
pub mod scope;
pub mod snapshot;
pub mod task;
pub mod verification;
pub mod verification_retention;
pub mod worktree;
pub mod worktree_salvage;

use std::path::{Path, PathBuf};

/// Resolve symlinks when possible while preserving paths that do not exist yet.
pub fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Stable filesystem-safe name for a repository identity.
pub fn repo_slug(value: &str) -> String {
    use sha2::Digest;

    let mut digest = sha2::Sha256::new();
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())[..12].to_string()
}

/// Atomically replace a durable coordination record.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let parent = path
        .parent()
        .context("record path has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".grove-record-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
