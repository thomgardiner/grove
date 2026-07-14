//! Smart routing: map a git diff to the affected workspace packages so a build or
//! test only touches what changed. Uses `cargo metadata` for the package layout and
//! the reverse-dependency graph, so it works on any Cargo workspace.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::Command;

/// Version of the machine-readable planning schema.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// One dependency-ordered group that an external orchestrator may run concurrently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanGroup {
    /// Packages that can run together after all preceding groups complete.
    pub packages: Vec<String>,
    /// Claim scopes corresponding to `packages`.
    pub claim_scopes: Vec<String>,
    /// A workspace-wide verification command should replace per-package commands.
    pub full_workspace: bool,
}

/// Stable impact and execution plan. `full` means a workspace/build contract changed;
/// its sole group asks for one workspace-wide verification command. Otherwise `packages`
/// is the reverse-dependency closure of the changed packages, ordered by `groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// Version of this serialized structure.
    pub schema_version: u32,
    /// Changed repository-relative paths used to make the plan.
    pub changed_files: Vec<String>,
    pub full: bool,
    pub packages: BTreeSet<String>,
    /// Dependency-ordered concurrent groups, in execution order.
    pub groups: Vec<PlanGroup>,
}

fn git(workspace: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Files changed vs `base` (default the working tree): committed since the
/// merge-base, plus uncommitted and untracked files, normalized to forward slashes.
pub fn changed_files(workspace: &Path, base: &str) -> Result<Vec<String>> {
    let merge_base = git(workspace, &["merge-base", base, "HEAD"])?
        .trim()
        .to_string();
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut add = |out: String| {
        for f in out.split('\0').filter(|s| !s.is_empty()) {
            files.insert(f.replace('\\', "/"));
        }
    };
    add(git(
        workspace,
        &["diff", "--name-only", "-z", &merge_base, "HEAD"],
    )?);
    add(git(workspace, &["diff", "--name-only", "-z", "HEAD"])?);
    add(git(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?);
    Ok(files.into_iter().collect())
}

fn is_build_contract(file: &str) -> bool {
    file == "Cargo.toml"
        || file == "Cargo.lock"
        || file == ".grove.toml"
        || file == "rust-toolchain"
        || file == "rust-toolchain.toml"
        || file.starts_with(".cargo/")
}

/// The package owning `file`, from `dirs` (package-relative-dir, name), which must be
/// sorted longest-dir-first so a nested package wins over an ancestor.
fn package_for_file<'a>(file: &str, dirs: &'a [(String, String)]) -> Option<&'a str> {
    dirs.iter()
        .find(|(dir, _)| dir == "." || file == dir || file.starts_with(&format!("{dir}/")))
        .map(|(_, name)| name.as_str())
}

/// The reverse-dependency closure of `changed`: every package that transitively
/// depends on a changed package, plus the changed packages themselves.
fn reverse_closure(
    changed: HashSet<String>,
    reverse: &HashMap<String, HashSet<String>>,
) -> BTreeSet<String> {
    let mut closure: BTreeSet<String> = changed.iter().cloned().collect();
    let mut pending: Vec<String> = changed.into_iter().collect();
    while let Some(dep) = pending.pop() {
        if let Some(dependents) = reverse.get(&dep) {
            for d in dependents {
                if closure.insert(d.clone()) {
                    pending.push(d.clone());
                }
            }
        }
    }
    closure
}

fn normalize_files(files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|file| !file.is_empty())
        .map(|file| file.replace('\\', "/"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn group(packages: Vec<String>, full_workspace: bool) -> PlanGroup {
    let claim_scopes = packages
        .iter()
        .map(|name| format!("crate:{name}"))
        .collect();
    PlanGroup {
        packages,
        claim_scopes,
        full_workspace,
    }
}

/// Topologically layer `packages` by their workspace dependencies. Every returned group
/// can run concurrently; groups must run in their returned order.
fn dependency_groups(
    packages: &BTreeSet<String>,
    dependencies: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Vec<PlanGroup>> {
    let mut done = BTreeSet::new();
    let mut pending = packages.clone();
    let mut groups = Vec::new();

    while !pending.is_empty() {
        let ready: Vec<_> = pending
            .iter()
            .filter(|name| {
                dependencies
                    .get(*name)
                    .into_iter()
                    .flatten()
                    .filter(|dependency| packages.contains(*dependency))
                    .all(|dependency| done.contains(dependency))
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            bail!(
                "workspace package dependency graph contains a cycle among {}",
                pending.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        for name in &ready {
            pending.remove(name);
            done.insert(name.clone());
        }
        groups.push(group(ready, false));
    }
    Ok(groups)
}

fn result(
    files: Vec<String>,
    full: bool,
    packages: BTreeSet<String>,
    groups: Vec<PlanGroup>,
) -> Plan {
    Plan {
        schema_version: PLAN_SCHEMA_VERSION,
        changed_files: files,
        full,
        packages,
        groups,
    }
}

/// The affected-package plan for a set of changed files.
pub fn plan(workspace: &Path, files: &[String]) -> Result<Plan> {
    let files = normalize_files(files);
    if files.is_empty() {
        return Ok(result(files, false, BTreeSet::new(), Vec::new()));
    }
    if files.iter().any(|f| is_build_contract(f)) {
        return Ok(result(
            files,
            true,
            BTreeSet::new(),
            vec![group(Vec::new(), true)],
        ));
    }

    let meta = cargo_metadata::MetadataCommand::new()
        .current_dir(workspace)
        .no_deps()
        .exec()
        .context("cargo metadata")?;
    let root = meta.workspace_root.as_std_path();
    let members: HashSet<_> = meta.workspace_members.iter().cloned().collect();
    let pkgs: Vec<_> = meta
        .packages
        .iter()
        .filter(|p| members.contains(&p.id))
        .collect();

    // Package directories, relative to the workspace root, longest first so a
    // nested package wins over an ancestor.
    let mut dirs: Vec<(String, String)> = pkgs
        .iter()
        .map(|p| {
            let dir = p.manifest_path.parent().unwrap().as_std_path();
            let rel = dir
                .strip_prefix(root)
                .unwrap_or(dir)
                .to_string_lossy()
                .replace('\\', "/");
            (
                if rel.is_empty() { ".".to_string() } else { rel },
                p.name.clone(),
            )
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0.len()));

    let names: HashSet<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
    let mut dependencies = BTreeMap::new();
    let mut reverse: HashMap<String, HashSet<String>> = HashMap::new();
    for p in &pkgs {
        let mut deps = BTreeSet::new();
        for dep in &p.dependencies {
            if names.contains(dep.name.as_str()) {
                deps.insert(dep.name.clone());
                reverse
                    .entry(dep.name.clone())
                    .or_default()
                    .insert(p.name.clone());
            }
        }
        dependencies.insert(p.name.clone(), deps);
    }

    let mut changed: HashSet<String> = HashSet::new();
    for file in &files {
        if let Some(name) = package_for_file(file, &dirs) {
            changed.insert(name.to_string());
        }
    }
    let packages = reverse_closure(changed, &reverse);
    let groups = dependency_groups(&packages, &dependencies)?;
    Ok(result(files, false, packages, groups))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(workspace: &Path, name: &str, dependencies: &str) {
        let dir = workspace.join("crates").join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{dependencies}"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    }

    #[test]
    fn build_contracts_force_a_full_rebuild() {
        for f in [
            "Cargo.toml",
            "Cargo.lock",
            ".grove.toml",
            "rust-toolchain.toml",
            ".cargo/config.toml",
        ] {
            assert!(is_build_contract(f), "{f} should be a build contract");
        }
        for f in ["crates/foo/src/lib.rs", "README.md", "src/main.rs"] {
            assert!(!is_build_contract(f), "{f} should not be a build contract");
        }
    }

    #[test]
    fn package_for_file_prefers_the_nested_package() {
        // Longest dir first, as workspaceGraph sorts them.
        let dirs = vec![
            ("crates/foo/bar".to_string(), "bar".to_string()),
            ("crates/foo".to_string(), "foo".to_string()),
        ];
        assert_eq!(
            package_for_file("crates/foo/bar/src/lib.rs", &dirs),
            Some("bar")
        );
        assert_eq!(
            package_for_file("crates/foo/src/lib.rs", &dirs),
            Some("foo")
        );
        assert_eq!(package_for_file("docs/x.md", &dirs), None);
    }

    #[test]
    fn reverse_closure_walks_all_dependents() {
        // a <- b <- c, and a <- d
        let mut reverse: HashMap<String, HashSet<String>> = HashMap::new();
        reverse.insert("a".into(), HashSet::from(["b".into(), "d".into()]));
        reverse.insert("b".into(), HashSet::from(["c".into()]));
        let got = reverse_closure(HashSet::from(["a".to_string()]), &reverse);
        assert_eq!(
            got,
            BTreeSet::from(["a".into(), "b".into(), "c".into(), "d".into()])
        );
        // A leaf with no dependents routes to only itself.
        let leaf = reverse_closure(HashSet::from(["c".to_string()]), &reverse);
        assert_eq!(leaf, BTreeSet::from(["c".to_string()]));
    }

    #[test]
    fn plan_groups_independent_leaves_before_their_dependent() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/api\", \"crates/core\", \"crates/model\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        package(workspace.path(), "core", "");
        package(workspace.path(), "model", "");
        package(
            workspace.path(),
            "api",
            "[dependencies]\ncore = { path = \"../core\" }\n",
        );

        let plan = plan(
            workspace.path(),
            &[
                "crates/core/src/lib.rs".to_string(),
                "crates/model/src/lib.rs".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(
            plan.packages,
            BTreeSet::from(["api", "core", "model"].map(String::from))
        );
        let groups = plan.groups;
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].packages, ["core", "model"]);
        assert_eq!(groups[0].claim_scopes, ["crate:core", "crate:model"]);
        assert!(!groups[0].full_workspace);
        assert_eq!(groups[1].packages, ["api"]);
    }

    #[test]
    fn workspace_configuration_requests_one_full_workspace_group() {
        let workspace = tempfile::tempdir().unwrap();
        let plan = plan(workspace.path(), &[".grove.toml".to_string()]).unwrap();

        assert_eq!(plan.schema_version, PLAN_SCHEMA_VERSION);
        assert_eq!(plan.changed_files, [".grove.toml"]);
        assert!(plan.full);
        assert!(plan.packages.is_empty());
        assert_eq!(plan.groups.len(), 1);
        assert!(plan.groups[0].full_workspace);
        assert!(plan.groups[0].packages.is_empty());
        assert_eq!(serde_json::to_value(&plan).unwrap()["schema_version"], 1);
    }
}
