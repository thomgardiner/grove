use super::{FallbackReason, MaterializationMode, MaterializationPlan, PLAN_SCHEMA_VERSION};
use crate::claim::claim_scope::{PackageIndex, PackagePath, normalize_scope};
use anyhow::{Context as _, Result, bail};
use cargo_metadata::{Metadata, Package, PackageId, Target};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

#[path = "materialization_source.rs"]
mod materialization_source;
#[path = "materialization_tree.rs"]
mod materialization_tree;
use super::materialization_cargo::Fingerprint;
use materialization_source::{exact, sparse, verify_cargo_config, verify_config, verify_inputs};
#[cfg(test)]
use materialization_tree::Entry;
use materialization_tree::{Metrics, Tree, covers, parent};

pub(crate) struct PlanInput<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) base_oid: &'a str,
    pub(crate) scopes: &'a [String],
    pub(crate) extras: &'a [String],
    pub(crate) config: Option<&'a Path>,
    pub(crate) fingerprint: &'a Fingerprint,
    pub(crate) planned_at: u64,
}

struct Planner<'a> {
    input: PlanInput<'a>,
    metadata: Metadata,
    index: PackageIndex,
    tree: Tree,
    full: Metrics,
    cargo_support: Vec<String>,
}

#[derive(Default)]
struct Selection {
    requested: BTreeSet<String>,
    packages: BTreeSet<PackageId>,
    closure: BTreeSet<String>,
    support: BTreeSet<String>,
    root: bool,
}

pub(crate) fn plan(input: PlanInput<'_>) -> Result<MaterializationPlan> {
    build(input, true)
}

pub(crate) fn expand(input: PlanInput<'_>) -> Result<MaterializationPlan> {
    build(input, false)
}

pub(crate) fn measure(workspace: &Path, base_oid: &str) -> Result<(u64, u64)> {
    let metrics = Tree::load(workspace, base_oid)?.full()?;
    Ok((metrics.working_files, metrics.working_logical_bytes))
}

fn build(input: PlanInput<'_>, clean: bool) -> Result<MaterializationPlan> {
    if input.base_oid.is_empty() || input.planned_at == 0 {
        bail!("materialization plan requires a base OID and timestamp")
    }
    check(&input, clean)?;
    let tree = Tree::load(input.workspace, input.base_oid)?;
    verify_config(&input, &tree)?;
    let cargo_support = verify_cargo_config(&tree, input.workspace)?;
    let full = tree.full()?;
    let mut command = cargo_metadata::MetadataCommand::new();
    command
        .current_dir(input.workspace)
        .other_options(vec!["--locked".into()]);
    let metadata = command
        .exec()
        .context("cargo metadata while planning materialization")?;
    check(&input, clean)?;
    let index = PackageIndex::new(&metadata, tree.root())?;
    verify_inputs(&metadata, &tree)?;
    let planner = Planner {
        input,
        metadata,
        index,
        tree,
        full,
        cargo_support,
    };
    let selection = select(&planner)?;
    finish(&planner, selection)
}

fn check(input: &PlanInput<'_>, clean: bool) -> Result<()> {
    if clean { exact(input) } else { sparse(input) }
}

fn select(planner: &Planner<'_>) -> Result<Selection> {
    let mut selected = Selection::default();
    for scope in planner.input.scopes {
        if let Some(name) = scope.strip_prefix("crate:") {
            let id = planner
                .index
                .workspace
                .get(name)
                .with_context(|| format!("no workspace crate named {name:?}"))?;
            selected.requested.insert(format!("crate:{name}"));
            selected.packages.insert(id.clone());
        } else {
            let scope = normalize_scope(scope)?;
            selected.requested.insert(scope.clone());
            add_cone(
                &mut selected.closure,
                planner.tree.cone(&scope)?,
                &mut selected.root,
            );
        }
    }
    dependencies(planner, &mut selected)?;
    package_cones(planner, &mut selected)?;
    extras(planner, &mut selected)?;
    Ok(selected)
}

fn dependencies(planner: &Planner<'_>, selected: &mut Selection) -> Result<()> {
    let resolve = planner
        .metadata
        .resolve
        .as_ref()
        .context("cargo metadata omitted the dependency graph")?;
    let mut pending: VecDeque<_> = selected.packages.iter().cloned().collect();
    while let Some(id) = pending.pop_front() {
        let node = resolve
            .nodes
            .iter()
            .find(|node| node.id == id)
            .with_context(|| format!("cargo metadata omitted dependency node {id}"))?;
        for dependency in &node.deps {
            if planner.index.paths.contains_key(&dependency.pkg)
                && selected.packages.insert(dependency.pkg.clone())
            {
                pending.push_back(dependency.pkg.clone());
            }
        }
    }
    Ok(())
}

fn package_cones(planner: &Planner<'_>, selected: &mut Selection) -> Result<()> {
    for package in planner
        .metadata
        .packages
        .iter()
        .filter(|package| planner.index.paths.contains_key(&package.id))
    {
        let closure = selected.packages.contains(&package.id);
        let path = planner
            .index
            .paths
            .get(&package.id)
            .context("local package missing from package index")?;
        match (closure, path) {
            (_, PackagePath::External) | (true, PackagePath::Root(_)) => selected.root = true,
            (true, PackagePath::Repo(path)) => closure_package(planner, selected, package, path)?,
            (false, path) => support_package(planner, selected, package, path)?,
        }
    }
    Ok(())
}

fn closure_package(
    planner: &Planner<'_>,
    selected: &mut Selection,
    package: &Package,
    package_dir: &str,
) -> Result<()> {
    selected.closure.insert(package_dir.into());
    for target in &package.targets {
        let Some(target_parent) = target_parent(&planner.tree, target)? else {
            selected.root = true;
            return Ok(());
        };
        if !covers(package_dir, &target_parent) {
            add_cone(&mut selected.closure, target_parent, &mut selected.root);
        }
    }
    Ok(())
}

fn support_package(
    planner: &Planner<'_>,
    selected: &mut Selection,
    package: &Package,
    package_path: &PackagePath,
) -> Result<()> {
    let package_dir = match package_path {
        PackagePath::Root(path) | PackagePath::Repo(path) => path,
        PackagePath::External => unreachable!(),
    };
    if package.targets.is_empty() {
        add_cone(
            &mut selected.support,
            package_dir.into(),
            &mut selected.root,
        );
    }
    for target in &package.targets {
        let Some(target_parent) = target_parent(&planner.tree, target)? else {
            selected.root = true;
            return Ok(());
        };
        if package_dir != "." && !covers(package_dir, &target_parent) {
            selected.support.insert(package_dir.into());
        }
        add_cone(&mut selected.support, target_parent, &mut selected.root);
    }
    Ok(())
}

fn target_parent(tree: &Tree, target: &Target) -> Result<Option<String>> {
    let target = match fs::canonicalize(target.src_path.as_std_path()) {
        Ok(target) => target,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("canonicalizing Cargo target {}", target.src_path));
        }
    };
    let Some(relative) = tree.relative(&target)? else {
        return Ok(None);
    };
    if !tree.contains(&relative) {
        bail!("Cargo target {relative:?} is not present at the selected base")
    }
    Ok(Some(parent(&relative)))
}

fn extras(planner: &Planner<'_>, selected: &mut Selection) -> Result<()> {
    for extra in planner.input.extras {
        let extra = normalize_scope(extra)?;
        let cone = planner.tree.cone(&extra)?;
        add_cone(&mut selected.support, cone, &mut selected.root);
    }
    for path in &planner.cargo_support {
        selected.support.insert(path.clone());
    }
    Ok(())
}

fn add_cone(cones: &mut BTreeSet<String>, cone: String, root: &mut bool) {
    if cone == "." {
        *root = true;
    } else {
        cones.insert(cone);
    }
}

fn minimize(closure: &BTreeSet<String>, support: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let mut all: Vec<_> = closure.union(support).cloned().collect();
    all.sort_by_key(|path| (path.matches('/').count(), path.clone()));
    let mut cones: Vec<String> = Vec::new();
    for path in all {
        if !cones.iter().any(|cone| covers(cone, &path)) {
            cones.push(path);
        }
    }
    let (mut closure, mut support): (Vec<_>, Vec<_>) = cones
        .into_iter()
        .partition(|cone| closure.iter().any(|path| covers(cone, path)));
    closure.sort();
    support.sort();
    (closure, support)
}

fn finish(planner: &Planner<'_>, selected: Selection) -> Result<MaterializationPlan> {
    let requested_scopes: Vec<_> = selected.requested.into_iter().collect();
    let closure_packages = package_names(&planner.metadata, &selected.packages);
    if requested_scopes.is_empty() {
        return Ok(full_plan(planner, requested_scopes, closure_packages, None));
    }
    if planner.input.fingerprint.hash.is_empty() {
        bail!("sparse materialization requires a Cargo fingerprint")
    }
    if selected.root {
        return Ok(full_plan(
            planner,
            requested_scopes,
            closure_packages,
            Some(FallbackReason::RootScope),
        ));
    }
    let (closure_cones, support_cones) = minimize(&selected.closure, &selected.support);
    let cones: Vec<_> = closure_cones
        .iter()
        .chain(&support_cones)
        .cloned()
        .collect();
    let metrics = planner.tree.metrics(&cones)?;
    if metrics == planner.full {
        return Ok(full_plan(
            planner,
            requested_scopes,
            closure_packages,
            Some(FallbackReason::NoReduction),
        ));
    }
    Ok(MaterializationPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        mode: MaterializationMode::Sparse,
        requested_scopes,
        closure_packages,
        closure_cones,
        support_cones,
        base_oid: planner.input.base_oid.into(),
        cargo_fingerprint: Some(planner.input.fingerprint.hash.clone()),
        full_tracked_files: planner.full.tracked_files,
        full_git_blob_bytes: planner.full.git_blob_bytes,
        selected_tracked_files: metrics.tracked_files,
        selected_git_blob_bytes: metrics.git_blob_bytes,
        full_working_files: planner.full.working_files,
        full_working_logical_bytes: planner.full.working_logical_bytes,
        selected_working_files: metrics.working_files,
        selected_working_logical_bytes: metrics.working_logical_bytes,
        fallback_reason: None,
        planned_at: planner.input.planned_at,
    })
}

fn package_names(metadata: &Metadata, ids: &BTreeSet<PackageId>) -> Vec<String> {
    metadata
        .packages
        .iter()
        .filter(|package| ids.contains(&package.id))
        .map(|package| package.name.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn full_plan(
    planner: &Planner<'_>,
    requested_scopes: Vec<String>,
    closure_packages: Vec<String>,
    reason: Option<FallbackReason>,
) -> MaterializationPlan {
    MaterializationPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        mode: MaterializationMode::Full,
        requested_scopes,
        closure_packages,
        closure_cones: Vec::new(),
        support_cones: Vec::new(),
        base_oid: planner.input.base_oid.into(),
        cargo_fingerprint: (!planner.input.fingerprint.hash.is_empty())
            .then(|| planner.input.fingerprint.hash.clone()),
        full_tracked_files: planner.full.tracked_files,
        full_git_blob_bytes: planner.full.git_blob_bytes,
        selected_tracked_files: planner.full.tracked_files,
        selected_git_blob_bytes: planner.full.git_blob_bytes,
        full_working_files: planner.full.working_files,
        full_working_logical_bytes: planner.full.working_logical_bytes,
        selected_working_files: planner.full.working_files,
        selected_working_logical_bytes: planner.full.working_logical_bytes,
        fallback_reason: reason,
        planned_at: planner.input.planned_at,
    }
}

#[cfg(test)]
#[path = "materialization_plan_tests.rs"]
mod tests;
