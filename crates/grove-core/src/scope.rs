//! Repository-relative scope validation shared by all ecosystem adapters.

use anyhow::{Result, bail};
use std::path::{Component, Path};

/// Normalize one repository-relative path scope without resolving ecosystem selectors.
pub fn normalize(scope: &str) -> Result<String> {
    let value = scope.replace('\\', "/");
    let path = Path::new(&value);
    let bytes = value.as_bytes();
    let windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if value.is_empty() || path.is_absolute() || windows_prefix {
        bail!("claim scope must be a nonempty repo-relative path")
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("claim scope must not escape the repository")
            }
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}
