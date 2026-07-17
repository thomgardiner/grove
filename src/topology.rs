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

struct Graph {
    /// name -> repo-relative dir ("." for root).
    dirs: BTreeMap<String, String>,
    /// name -> direct workspace deps.
    deps: BTreeMap<String, BTreeSet<String>>,
    root_has_members: bool,
}

fn graph(workspace: &Path) -> Result<Graph> {
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
        names.insert(package.name.clone());
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
        dirs.insert(package.name.clone(), relative);
        deps.insert(
            package.name.clone(),
            package
                .dependencies
                .iter()
                .filter(|dep| names.contains(&dep.name))
                .map(|dep| dep.name.clone())
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
fn owning_package(graph: &Graph, path: &str) -> Option<String> {
    graph
        .dirs
        .iter()
        .filter(|(_, dir)| *dir == "." || claim::path_overlap(dir, path))
        .max_by_key(|(_, dir)| if *dir == "." { 0 } else { dir.len() + 1 })
        .map(|(name, _)| name.clone())
}

fn transitive_deps(graph: &Graph, name: &str) -> BTreeSet<String> {
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

pub fn partition(workspace: &Path, sets: &[ScopeSet]) -> Result<Partition> {
    // Path-only partitions must work in any repository; the package graph is
    // optional intelligence layered on top when a Cargo workspace exists.
    let graph = graph(workspace).ok();
    let mut resolved_sets = Vec::new();
    for set in sets {
        let resolved = claim::resolve_scopes(workspace, &set.scope)?;
        let packages: BTreeSet<String> = graph
            .as_ref()
            .map(|graph| {
                resolved
                    .iter()
                    .filter_map(|path| owning_package(graph, path))
                    .collect()
            })
            .unwrap_or_default();
        resolved_sets.push(ResolvedSet {
            id: set.id.clone(),
            scope: set.scope.clone(),
            resolved,
            packages: packages.into_iter().collect(),
        });
    }

    let mut conflicts = Vec::new();
    let mut couplings = Vec::new();
    // Edges as (from, to): `to` runs after `from`.
    let mut edges: BTreeSet<(usize, usize)> = BTreeSet::new();
    for a in 0..resolved_sets.len() {
        for b in (a + 1)..resolved_sets.len() {
            let (first, second) = (&resolved_sets[a], &resolved_sets[b]);
            let overlap: BTreeSet<String> = first
                .resolved
                .iter()
                .flat_map(|x| {
                    second
                        .resolved
                        .iter()
                        .filter(|y| claim::path_overlap(x, y))
                        .flat_map(move |y| [x.clone(), y.clone()])
                })
                .collect();
            if !overlap.is_empty() {
                conflicts.push(Conflict {
                    a: first.id.clone(),
                    b: second.id.clone(),
                    overlap: overlap.into_iter().collect(),
                });
                // Serialize the pair (input order) so waves stay valid even
                // if the orchestrator proceeds without merging them.
                edges.insert((a, b));
                continue;
            }
            let Some(graph) = &graph else { continue };
            // Dependency coupling: if any of one side's packages is a
            // transitive dependency of the other's, the dependent runs later.
            let a_pkgs: BTreeSet<_> = first.packages.iter().cloned().collect();
            let b_pkgs: BTreeSet<_> = second.packages.iter().cloned().collect();
            let b_needs_a: Vec<String> = b_pkgs
                .iter()
                .flat_map(|p| transitive_deps(graph, p))
                .filter(|dep| a_pkgs.contains(dep))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let a_needs_b: Vec<String> = a_pkgs
                .iter()
                .flat_map(|p| transitive_deps(graph, p))
                .filter(|dep| b_pkgs.contains(dep))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let shared: Vec<String> = a_pkgs.intersection(&b_pkgs).cloned().collect();
            if !b_needs_a.is_empty() {
                couplings.push(Coupling {
                    upstream: first.id.clone(),
                    downstream: second.id.clone(),
                    kind: "dependency",
                    via: b_needs_a,
                });
                edges.insert((a, b));
            } else if !a_needs_b.is_empty() {
                couplings.push(Coupling {
                    upstream: second.id.clone(),
                    downstream: first.id.clone(),
                    kind: "dependency",
                    via: a_needs_b,
                });
                edges.insert((b, a));
            } else if !shared.is_empty() {
                couplings.push(Coupling {
                    upstream: first.id.clone(),
                    downstream: second.id.clone(),
                    kind: "same_package",
                    via: shared,
                });
                edges.insert((a, b));
            }
        }
    }

    // Kahn layering over the suggestion edges; cargo's package graph is
    // acyclic and input-order edges only ever point forward, so this drains.
    let mut waves = Vec::new();
    let mut remaining: BTreeSet<usize> = (0..resolved_sets.len()).collect();
    while !remaining.is_empty() {
        let ready: Vec<usize> = remaining
            .iter()
            .copied()
            .filter(|&set| {
                !edges
                    .iter()
                    .any(|(from, to)| *to == set && remaining.contains(from))
            })
            .collect();
        if ready.is_empty() {
            // Unreachable by construction; drain deterministically anyway.
            waves.push(
                remaining
                    .iter()
                    .map(|&i| resolved_sets[i].id.clone())
                    .collect(),
            );
            break;
        }
        waves.push(ready.iter().map(|&i| resolved_sets[i].id.clone()).collect());
        for index in ready {
            remaining.remove(&index);
        }
    }

    let suggested_after = resolved_sets
        .iter()
        .enumerate()
        .filter_map(|(index, set)| {
            let after: Vec<String> = edges
                .iter()
                .filter(|(_, to)| *to == index)
                .map(|(from, _)| resolved_sets[*from].id.clone())
                .collect();
            (!after.is_empty()).then(|| AfterEdge {
                id: set.id.clone(),
                after,
            })
        })
        .collect();

    Ok(Partition {
        schema_version: PARTITION_SCHEMA_VERSION,
        sets: resolved_sets,
        conflicts,
        couplings,
        waves,
        suggested_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Workspace with `core` and `app`, where app depends on core by path.
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let base = tempdir().unwrap();
        let root = base.path().join("ws");
        for (name, extra) in [
            ("core", String::new()),
            ("app", "core = { path = \"../core\" }\n".to_string()),
        ] {
            let dir = root.join("crates").join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{extra}"
                ),
            )
            .unwrap();
            fs::write(dir.join("src/lib.rs"), "").unwrap();
        }
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/core\", \"crates/app\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        let repo = root.clone();
        crate::git::run(&repo, &["init", "-q"]).unwrap();
        (base, root)
    }

    fn set(id: &str, scope: &[&str]) -> ScopeSet {
        ScopeSet {
            id: id.to_string(),
            scope: scope.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn topology_maps_packages_edges_and_claim_scopes() {
        let (_base, root) = workspace();
        let topology = topology(&root).unwrap();
        assert_eq!(topology.packages.len(), 2);
        let app = topology.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.path, "crates/app");
        assert_eq!(app.claim_scope.as_deref(), Some("crate:app"));
        assert_eq!(app.deps, ["core"]);
        let core = topology.packages.iter().find(|p| p.name == "core").unwrap();
        assert_eq!(core.dependents, ["app"]);
        assert!(core.deps.is_empty());
    }

    #[test]
    fn partition_reports_conflicts_couplings_and_waves() {
        let (_base, root) = workspace();
        let partition = partition(
            &root,
            &[
                set("core-work", &["crate:core"]),
                set("app-work", &["crate:app"]),
                set("core-clash", &["crates/core/src"]),
            ],
        )
        .unwrap();

        // crate:core vs a path inside it: a real claim conflict.
        assert_eq!(partition.conflicts.len(), 1);
        assert_eq!(partition.conflicts[0].a, "core-work");
        assert_eq!(partition.conflicts[0].b, "core-clash");

        // app depends on core: dependency coupling, app after core.
        let dep = partition
            .couplings
            .iter()
            .find(|c| c.kind == "dependency")
            .unwrap();
        assert_eq!(dep.upstream, "core-work");
        assert_eq!(dep.downstream, "app-work");
        assert_eq!(dep.via, ["core"]);

        // Waves: both core-touching sets serialize first (the conflict pair in
        // input order), and app — a dependent of core — runs after both.
        assert_eq!(
            partition.waves,
            [["core-work"], ["core-clash"], ["app-work"]]
        );

        let after: Vec<_> = partition
            .suggested_after
            .iter()
            .map(|e| (e.id.as_str(), e.after.clone()))
            .collect();
        assert!(
            after.contains(&(
                "app-work",
                vec!["core-work".to_string(), "core-clash".to_string()]
            )),
            "{after:?}"
        );
    }

    #[test]
    fn same_package_sets_couple_in_input_order() {
        let (_base, root) = workspace();
        let partition = partition(
            &root,
            &[
                set("first", &["crates/core/src/lib.rs"]),
                set("second", &["crates/core/Cargo.toml"]),
            ],
        )
        .unwrap();
        assert!(partition.conflicts.is_empty());
        let coupling = &partition.couplings[0];
        assert_eq!(coupling.kind, "same_package");
        assert_eq!(coupling.upstream, "first");
        assert_eq!(coupling.downstream, "second");
        assert_eq!(partition.waves, [["first"], ["second"]]);
    }
}
