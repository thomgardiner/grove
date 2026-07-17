//! Cargo configuration checks for the restricted portable-receipt contract.

use anyhow::Result;
use std::path::Path;

use super::{Document, load};

/// Portable verification excludes Cargo features that pull opaque executable or source
/// inputs from outside the captured repository.
pub(super) fn supported(workspace: &Path) -> Result<bool> {
    Ok(load(workspace)?.configs.iter().all(document_supported))
}

fn document_supported(document: &Document) -> bool {
    if document.value.get("paths").is_some()
        || document.value.get("env").is_some()
        || document.value.get("source").is_some()
        || target_tools(document)
    {
        return false;
    }
    let Some(build) = document.value.get("build").and_then(toml::Value::as_table) else {
        return true;
    };
    ![
        "rustc",
        "rustc-wrapper",
        "rustc-workspace-wrapper",
        "rustdoc",
        "target-dir",
        "build-dir",
    ]
    .iter()
    .any(|key| build.contains_key(*key))
}

fn target_tools(document: &Document) -> bool {
    document
        .value
        .get("target")
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets.values().any(|target| {
                target.as_table().is_some_and(|target| {
                    target.contains_key("linker")
                        || target.contains_key("runner")
                        || target.contains_key("rustflags")
                })
            })
        })
}
