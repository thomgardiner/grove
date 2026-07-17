use super::{PlanInput, Tree};
use crate::git;
use anyhow::{Context as _, Result, bail};
use cargo_metadata::Metadata;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) fn exact(input: &PlanInput<'_>) -> Result<()> {
    let head = git::capture(input.workspace, &["rev-parse", "HEAD"])?;
    if head != input.base_oid {
        bail!("materialization source HEAD does not match the selected base")
    }
    let root = repository_root(input.workspace)?;
    reject_hidden_index_flags(&root, false)?;
    reject_ignored_inputs(&root)?;
    reject_tracked_manifest_symlinks(&root)?;
    reject_ignored_locks(input.workspace, &root)?;
    let status = git::capture(
        input.workspace,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if !status.is_empty() {
        bail!("materialization source must be clean at the selected base")
    }
    Ok(())
}

pub(super) fn sparse(input: &PlanInput<'_>) -> Result<()> {
    let root = repository_root(input.workspace)?;
    reject_hidden_index_flags(&root, true)?;
    reject_ignored_inputs(&root)?;
    reject_tracked_manifest_symlinks(&root)?;
    reject_ignored_locks(input.workspace, &root)
}

fn reject_tracked_manifest_symlinks(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--", ":(top,glob)**/Cargo.toml"])
        .current_dir(root)
        .output()
        .context("spawning git ls-files for tracked Cargo manifests")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed while checking tracked Cargo manifests: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let paths = String::from_utf8(output.stdout).context("Git manifest paths are not UTF-8")?;
    for path in paths.split_terminator('\0') {
        reject_final_symlink(&root.join(path), "Cargo manifest")?;
    }
    Ok(())
}

fn repository_root(workspace: &Path) -> Result<PathBuf> {
    let root = git::capture(workspace, &["rev-parse", "--show-toplevel"])?;
    fs::canonicalize(root).context("canonicalizing Git repository root")
}

fn reject_ignored_inputs(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
            ":(top,glob)**/Cargo.toml",
            ":(top,glob)**/Cargo.lock",
            ":(top,glob)**/.cargo/**",
            ":(top,glob)**/rust-toolchain",
            ":(top,glob)**/rust-toolchain.toml",
            ":(top,glob)**/.grove.toml",
        ])
        .current_dir(root)
        .output()
        .context("spawning git ls-files for ignored materialization inputs")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed while checking ignored materialization inputs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    if !output.stdout.is_empty() {
        bail!("materialization source has ignored Cargo or Grove inputs")
    }
    Ok(())
}

fn reject_ignored_locks(workspace: &Path, root: &Path) -> Result<()> {
    let mut dir = fs::canonicalize(workspace).context("canonicalizing Cargo working directory")?;
    loop {
        if ignored(root, &dir.join("Cargo.lock"))? {
            bail!("materialization source ignores a prospective Cargo lockfile")
        }
        if dir == root {
            return Ok(());
        }
        dir = dir
            .parent()
            .filter(|parent| parent.starts_with(root))
            .context("Cargo working directory is outside the Git repository")?
            .to_path_buf();
    }
}

fn ignored(root: &Path, path: &Path) -> Result<bool> {
    let status = Command::new("git")
        .args(["check-ignore", "--no-index", "-q", "--"])
        .arg(path)
        .current_dir(root)
        .status()
        .context("spawning git check-ignore for materialization planning")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git check-ignore failed while planning materialization"),
    }
}

fn reject_hidden_index_flags(root: &Path, allow_sparse: bool) -> Result<()> {
    let output = Command::new("git")
        .args(["ls-files", "-v", "-z"])
        .current_dir(root)
        .output()
        .context("spawning git ls-files for materialization planning")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed while planning materialization: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    if output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|record| record.first())
        .any(|tag| (!allow_sparse && *tag == b'S') || tag.is_ascii_lowercase())
    {
        bail!("materialization source has hidden Git index flags")
    }
    Ok(())
}

pub(super) fn verify_inputs(metadata: &Metadata, tree: &Tree) -> Result<()> {
    let workspace = fs::canonicalize(metadata.workspace_root.as_std_path())
        .context("canonicalizing Cargo workspace root")?;
    require_base_file(tree, &workspace.join("Cargo.toml"), "workspace manifest")?;
    // Inclusion is decided by where the manifest really lives, not by package
    // index membership. The metadata carries the full dependency graph:
    // registry and git dependencies canonicalize outside the repository and
    // are skipped, while vendored directory-source packages carry registry
    // source IDs (so the index never classifies them) yet live in-repo and
    // must be present at the base like any other in-repo manifest.
    for package in &metadata.packages {
        let manifest =
            fs::canonicalize(package.manifest_path.as_std_path()).with_context(|| {
                format!("canonicalizing package manifest {}", package.manifest_path)
            })?;
        if manifest.starts_with(tree.root()) {
            require_base_file(tree, &manifest, "package manifest")?;
        }
    }
    require_if_present(tree, &workspace.join("Cargo.lock"), "Cargo lockfile")?;
    Ok(())
}

pub(super) fn verify_config(input: &PlanInput<'_>, tree: &Tree) -> Result<()> {
    if input.extras.is_empty() {
        return Ok(());
    }
    let config = input
        .config
        .context("materialization extras require a repository config")?;
    reject_final_symlink(config, "Grove repository config")?;
    let config = fs::canonicalize(config).context("canonicalizing Grove repository config")?;
    require_base_file(tree, &config, "Grove repository config").map(drop)
}

pub(super) fn verify_cargo_config(tree: &Tree, workspace: &Path) -> Result<Vec<String>> {
    let mut dir = fs::canonicalize(workspace).context("canonicalizing Cargo working directory")?;
    let mut support = BTreeSet::new();
    let mut visited = BTreeSet::new();
    loop {
        for path in ["Cargo.toml", "Cargo.lock"] {
            reject_if_present(tree.root(), &dir.join(path), "Cargo input")?;
        }
        for path in [".cargo/config", ".cargo/config.toml"] {
            verify_config_file(tree, &dir.join(path), &mut support, &mut visited)?;
        }
        for path in ["rust-toolchain", "rust-toolchain.toml"] {
            require_if_present(tree, &dir.join(path), "Cargo configuration")?;
        }
        let cargo = dir.join(".cargo");
        if cargo.is_dir()
            && let Some(relative) = tree.relative(&cargo)?
            && tree.dirs.contains(&relative)
        {
            support.insert(relative);
        }
        if dir == tree.root() {
            return Ok(support.into_iter().collect());
        }
        dir = dir
            .parent()
            .filter(|parent| parent.starts_with(tree.root()))
            .context("Cargo working directory is outside the Git repository")?
            .to_path_buf();
    }
}

fn verify_config_file(
    tree: &Tree,
    path: &Path,
    support: &mut BTreeSet<String>,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if !path
        .try_exists()
        .with_context(|| format!("checking {}", path.display()))?
    {
        return Ok(());
    }
    reject_symlink_components(tree.root(), path, "Cargo configuration")?;
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalizing Cargo config {}", path.display()))?;
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    let relative = require_base_file(tree, &canonical, "Cargo configuration")?;
    if relative.contains('/') {
        support.insert(tree.cone(&relative)?);
    }
    let value: toml::Value = fs::read_to_string(&canonical)
        .with_context(|| format!("reading Cargo config {}", canonical.display()))?
        .parse()
        .with_context(|| format!("parsing Cargo config {}", canonical.display()))?;
    for (include, optional) in includes(&value)? {
        let include = canonical
            .parent()
            .context("Cargo config has no parent directory")?
            .join(include);
        if optional && !include.try_exists()? {
            continue;
        }
        verify_config_file(tree, &include, support, visited)?;
    }
    Ok(())
}

fn reject_if_present(root: &Path, path: &Path, kind: &str) -> Result<()> {
    if path
        .try_exists()
        .with_context(|| format!("checking {}", path.display()))?
    {
        reject_symlink_components(root, path, kind)?;
    }
    Ok(())
}

fn reject_final_symlink(path: &Path, kind: &str) -> Result<()> {
    if fs::symlink_metadata(path)
        .with_context(|| format!("reading {kind} metadata for {}", path.display()))?
        .file_type()
        .is_symlink()
    {
        bail!("{kind} must not be a symlink")
    }
    Ok(())
}

fn reject_symlink_components(root: &Path, path: &Path, kind: &str) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{kind} {} is outside the repository", path.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => current.push(part),
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => {
                if current == root || !current.pop() {
                    bail!("{kind} escapes the repository")
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("{kind} has an invalid repository path")
            }
        }
        reject_final_symlink(&current, kind)?;
    }
    Ok(())
}

fn includes(value: &toml::Value) -> Result<Vec<(String, bool)>> {
    let Some(include) = value.get("include") else {
        return Ok(Vec::new());
    };
    let items = include
        .as_array()
        .context("Cargo config include must be an array")?;
    items
        .iter()
        .map(|item| {
            if let Some(path) = item.as_str() {
                return Ok((path.into(), false));
            }
            let table = item
                .as_table()
                .context("Cargo config include must be a path or table")?;
            let path = table
                .get("path")
                .and_then(toml::Value::as_str)
                .context("Cargo config include table requires a path")?;
            let optional = match table.get("optional") {
                Some(value) => value
                    .as_bool()
                    .context("Cargo config include optional must be a boolean")?,
                None => false,
            };
            Ok((path.into(), optional))
        })
        .collect()
}

fn require_if_present(tree: &Tree, path: &Path, kind: &str) -> Result<()> {
    if path
        .try_exists()
        .with_context(|| format!("checking {}", path.display()))?
    {
        require_base_file(tree, path, kind)?;
    }
    Ok(())
}

fn require_base_file(tree: &Tree, path: &Path, kind: &str) -> Result<String> {
    reject_symlink_components(tree.root(), path, kind)?;
    let relative = tree
        .relative(path)?
        .with_context(|| format!("{kind} {} is outside the repository", path.display()))?;
    if !tree.contains(&relative) {
        bail!("{kind} {relative:?} is not present at the selected base")
    }
    Ok(relative)
}
