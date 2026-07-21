use crate::git;
use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, PackageId};
pub(crate) use grove_core::scope::normalize as normalize_scope;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PackagePath {
    Root(String),
    Repo(String),
    External,
}

pub(crate) struct PackageIndex {
    pub(crate) members: usize,
    pub(crate) workspace: BTreeMap<String, PackageId>,
    pub(crate) paths: BTreeMap<PackageId, PackagePath>,
}

impl PackageIndex {
    pub(crate) fn new(metadata: &Metadata, repo: &Path) -> Result<Self> {
        let workspace_root = fs::canonicalize(metadata.workspace_root.as_std_path())
            .context("canonicalizing Cargo workspace root")?;
        let repo_root = fs::canonicalize(repo).context("canonicalizing Git repository root")?;
        let mut paths = BTreeMap::new();
        for package in metadata
            .packages
            .iter()
            .filter(|package| package.source.is_none())
        {
            let dir = fs::canonicalize(
                package
                    .manifest_path
                    .parent()
                    .context("package manifest has no parent")?
                    .as_std_path(),
            )
            .with_context(|| {
                format!("canonicalizing package manifest {}", package.manifest_path)
            })?;
            let path = match dir.strip_prefix(&repo_root) {
                Ok(relative) if dir == workspace_root => PackagePath::Root(repo_path(relative)?),
                Ok(relative) => PackagePath::Repo(repo_path(relative)?),
                Err(_) => PackagePath::External,
            };
            paths.insert(package.id.clone(), path);
        }
        let mut workspace = BTreeMap::new();
        for id in &metadata.workspace_members {
            let package = metadata
                .packages
                .iter()
                .find(|package| &package.id == id)
                .with_context(|| format!("workspace package {id} missing from cargo metadata"))?;
            if workspace
                .insert(package.name.to_string(), id.clone())
                .is_some()
            {
                bail!("duplicate workspace crate named {:?}", package.name)
            }
        }
        Ok(Self {
            members: workspace.len(),
            workspace,
            paths,
        })
    }
}

/// Resolve every requested scope to one repo-relative path namespace. A non-root
/// `crate:name` owns its package directory. A sole root package owns the whole
/// workspace; a root package with nested members is ambiguous and must be explicit.
pub fn resolve_scopes(workspace: &Path, scopes: &[String]) -> Result<Vec<String>> {
    if scopes.iter().any(|scope| scope.starts_with("crate:"))
        && !crate::project::is_cargo_workspace(workspace)
    {
        bail!(
            "crate:<name> scopes need a Cargo workspace; use repo-relative path scopes in this repository"
        );
    }
    let packages = if scopes.iter().any(|scope| scope.starts_with("crate:")) {
        Some(package_index(workspace)?)
    } else {
        None
    };
    let mut resolved = BTreeSet::new();
    for scope in scopes {
        match scope.strip_prefix("crate:") {
            Some(name) => {
                let packages = packages
                    .as_ref()
                    .expect("package index exists for crate scopes");
                let id = packages
                    .workspace
                    .get(name)
                    .with_context(|| format!("no workspace crate named {name:?}"))?;
                match packages
                    .paths
                    .get(id)
                    .context("workspace package missing from package index")?
                {
                    PackagePath::Repo(path) => {
                        resolved.insert(path.clone());
                    }
                    PackagePath::Root(path) if packages.members == 1 => {
                        resolved.insert(path.clone());
                    }
                    PackagePath::Root(_) => {
                        bail!(
                            "workspace root crate {name:?} shares the workspace with nested members; \
                             claim explicit repo-relative paths for that package, or claim `.` for the \
                             whole workspace"
                        )
                    }
                    PackagePath::External => {
                        bail!("workspace package is outside the repository root")
                    }
                }
            }
            None => {
                resolved.insert(normalize_scope(scope)?);
            }
        }
    }
    Ok(resolved.into_iter().collect())
}

fn package_index(workspace: &Path) -> Result<PackageIndex> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(workspace)
        .no_deps()
        .exec()
        .context("cargo metadata while resolving claim scopes")?;
    PackageIndex::new(&metadata, &repository_root(workspace))
}

fn repo_path(relative: &Path) -> Result<String> {
    if relative.as_os_str().is_empty() {
        Ok(".".into())
    } else {
        normalize_scope(&relative.to_string_lossy())
    }
}

fn repository_root(workspace: &Path) -> PathBuf {
    git::capture(workspace, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace.to_path_buf())
}
