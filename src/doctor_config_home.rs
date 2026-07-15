//! Cargo-home resolution relative to the verification workspace.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub(super) fn path(workspace: &Path, configured: Option<OsString>) -> PathBuf {
    configured
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .unwrap_or_else(|| {
            crate::config::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".cargo")
        })
}

#[cfg(test)]
mod tests {
    use super::path;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn resolves_relative_cargo_home_from_the_workspace() {
        let workspace = Path::new("workspace");
        assert_eq!(
            path(workspace, Some(OsString::from("cargo-home"))),
            workspace.join("cargo-home")
        );
    }
}
