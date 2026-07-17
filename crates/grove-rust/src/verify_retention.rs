//! Rust-specific retention pins for portable Cargo evidence.

use std::collections::BTreeSet;
use std::path::Path;

#[path = "verify_retention_portable.rs"]
mod retention_portable;

pub(super) fn portable_runs(root: &Path) -> BTreeSet<(String, String)> {
    retention_portable::runs(root)
}
