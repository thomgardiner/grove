//! Workspace-local dependency check for portable verification receipts.

use anyhow::{Context, Result};
use std::path::Path;

use crate::{cache, snapshot};

/// Path dependencies are safe only when their manifests live under the captured
/// workspace. Registry and Git dependencies have a Cargo source identity; a source-less
/// package outside the workspace would otherwise be mutable receipt input.
pub(super) fn supported(workspace: &Path) -> Result<bool> {
    let workspace = cache::canonical_path(workspace);
    let Ok(snapshot) = snapshot::capture(&workspace) else {
        return Ok(false);
    };
    if snapshot::validate_frozen_links(&workspace, &snapshot).is_err() {
        return Ok(false);
    }
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(&workspace)
        .exec()
        .context("reading Cargo metadata for portable verification")?;
    Ok(metadata
        .packages
        .iter()
        .filter(|package| package.source.is_none())
        .all(|package| {
            package
                .manifest_path
                .parent()
                .map(|path| cache::canonical_path(path.as_std_path()).starts_with(&workspace))
                .unwrap_or(false)
        }))
}

#[cfg(test)]
mod tests {
    use super::supported;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    #[test]
    fn rejects_a_path_dependency_outside_the_workspace() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let external = root.path().join("external");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::create_dir_all(external.join("src")).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname='inside'\nversion='0.1.0'\nedition='2021'\n[dependencies]\noutside={path='../external'}\n",
        )
        .unwrap();
        fs::write(workspace.join("src/lib.rs"), "").unwrap();
        fs::write(
            external.join("Cargo.toml"),
            "[package]\nname='outside'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(external.join("src/lib.rs"), "").unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );

        assert!(!supported(&workspace).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_escaping_and_dangling_tracked_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let external = root.path().join("external.rs");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname='inside'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(&external, "pub fn external() {}\n").unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );

        symlink(&external, workspace.join("src/lib.rs")).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "-A"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
        assert!(!supported(&workspace).unwrap());

        fs::remove_file(workspace.join("src/lib.rs")).unwrap();
        symlink("missing.rs", workspace.join("src/lib.rs")).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "-A"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
        assert!(!supported(&workspace).unwrap());
    }
}
