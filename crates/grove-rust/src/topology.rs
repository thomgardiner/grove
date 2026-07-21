//! Forward-looking workspace intelligence for orchestrators. `topology` is
//! the map an LLM decomposes against: workspace packages, their dependency
//! edges, and the claim scope that owns each. `partition` is the refutation
//! step: proposed scope sets in, hard conflicts, build couplings, and
//! suggested execution waves out — using the exact scope resolution and
//! overlap semantics `task begin` will enforce, so the analysis can never
//! disagree with what actually blocks.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::claim;

pub const TOPOLOGY_SCHEMA_VERSION: u32 = 1;
pub const PARTITION_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
pub struct Topology {
    pub schema_version: u32,
    pub packages: Vec<PackageNode>,
}

#[derive(Serialize)]
pub struct PackageNode {
    pub name: String,
    /// Repo-relative package directory ("." for a root package).
    pub path: String,
    /// What an order's scope entry should say to claim this package; absent
    /// for a root package with nested members, which is not representable as
    /// one positive prefix (use explicit paths there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_scope: Option<String>,
    /// Direct workspace-internal dependencies.
    pub deps: Vec<String>,
    /// Workspace members that directly depend on this one.
    pub dependents: Vec<String>,
}

#[derive(Deserialize, Clone)]
pub struct ScopeSet {
    pub id: String,
    pub scope: Vec<String>,
    /// Sets sharing a group deliberately overlap (N-version attempts racing
    /// one order under a shared claim group); partition analysis treats them
    /// as neither conflicting nor ordering each other, mirroring the claim
    /// registry's group semantics.
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Serialize)]
pub struct Partition {
    pub schema_version: u32,
    pub sets: Vec<ResolvedSet>,
    /// Pairs whose resolved scopes overlap: the later `task begin` WILL block.
    pub conflicts: Vec<Conflict>,
    /// Pairs without conflicts that still influence each other through the
    /// package graph; concurrent execution risks rework and rebuild churn.
    pub couplings: Vec<Coupling>,
    /// Suggested concurrency batches honoring couplings and serializing
    /// conflicts; each inner list can run at once.
    pub waves: Vec<Vec<String>>,
    /// The wave structure as order-file edges an orchestrator can paste in.
    pub suggested_after: Vec<AfterEdge>,
}

#[derive(Serialize)]
pub struct ResolvedSet {
    pub id: String,
    pub scope: Vec<String>,
    pub resolved: Vec<String>,
    /// Workspace packages this set's resolved paths fall into.
    pub packages: Vec<String>,
}

#[derive(Serialize)]
pub struct Conflict {
    pub a: String,
    pub b: String,
    /// The overlapping resolved entries, from both sides.
    pub overlap: Vec<String>,
}

#[derive(Serialize)]
pub struct Coupling {
    pub upstream: String,
    pub downstream: String,
    /// "dependency": downstream's packages depend on upstream's.
    /// "same_package": both sets touch one package; ordered by input order.
    pub kind: &'static str,
    pub via: Vec<String>,
}

#[derive(Serialize)]
pub struct AfterEdge {
    pub id: String,
    pub after: Vec<String>,
}

pub(crate) struct Graph {
    /// name -> repo-relative dir ("." for root).
    dirs: BTreeMap<String, String>,
    /// name -> direct workspace deps.
    deps: BTreeMap<String, BTreeSet<String>>,
    root_has_members: bool,
}

pub(crate) fn graph(workspace: &Path) -> Result<Graph> {
    let meta = cargo_metadata::MetadataCommand::new()
        .current_dir(workspace)
        .no_deps()
        .exec()
        .context("cargo metadata")?;
    let root = meta.workspace_root.as_std_path().to_path_buf();
    let members: BTreeSet<_> = meta.workspace_members.iter().cloned().collect();
    let mut names = BTreeSet::new();
    let mut dirs = BTreeMap::new();
    let mut deps = BTreeMap::new();
    for package in meta.packages.iter().filter(|p| members.contains(&p.id)) {
        names.insert(package.name.to_string());
    }
    for package in meta.packages.iter().filter(|p| members.contains(&p.id)) {
        let dir = package
            .manifest_path
            .parent()
            .context("package manifest has no parent")?
            .as_std_path();
        let relative = dir
            .strip_prefix(&root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".to_string());
        dirs.insert(package.name.to_string(), relative);
        deps.insert(
            package.name.to_string(),
            package
                .dependencies
                .iter()
                .filter(|dep| names.contains(dep.name.as_str()))
                .map(|dep| dep.name.to_string())
                .collect(),
        );
    }
    let root_has_members = dirs.values().any(|d| d == ".") && dirs.len() > 1;
    Ok(Graph {
        dirs,
        deps,
        root_has_members,
    })
}

pub fn topology(workspace: &Path) -> Result<Topology> {
    let graph = graph(workspace)?;
    let mut dependents: BTreeMap<&String, Vec<String>> = BTreeMap::new();
    for (name, deps) in &graph.deps {
        for dep in deps {
            dependents.entry(dep).or_default().push(name.clone());
        }
    }
    let packages = graph
        .dirs
        .iter()
        .map(|(name, dir)| {
            let root_with_members = dir == "." && graph.root_has_members;
            PackageNode {
                name: name.clone(),
                path: dir.clone(),
                claim_scope: (!root_with_members).then(|| format!("crate:{name}")),
                deps: graph.deps[name].iter().cloned().collect(),
                dependents: dependents.get(name).cloned().unwrap_or_default(),
            }
        })
        .collect();
    Ok(Topology {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        packages,
    })
}

/// The longest package directory that owns `path`, mirroring how claims and
/// builds attribute a file to a crate.
pub(crate) fn owning_package(graph: &Graph, path: &str) -> Option<String> {
    graph
        .dirs
        .iter()
        .filter(|(_, dir)| *dir == "." || claim::path_overlap(dir, path))
        .max_by_key(|(_, dir)| if *dir == "." { 0 } else { dir.len() + 1 })
        .map(|(name, _)| name.clone())
}

pub(crate) fn transitive_deps(graph: &Graph, name: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<&String> = graph.deps.get(name).into_iter().flatten().collect();
    while let Some(dep) = queue.pop() {
        if seen.insert(dep.clone())
            && let Some(next) = graph.deps.get(dep)
        {
            queue.extend(next);
        }
    }
    seen
}

pub use crate::topology_partition::partition;
