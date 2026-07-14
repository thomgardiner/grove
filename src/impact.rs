//! Smart routing: map a git diff to the affected workspace packages so a build or
//! test only touches what changed. Uses `cargo metadata` for the package layout and
//! the reverse-dependency graph, so it works on any Cargo workspace.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::Command;

/// What to build. `full` means a workspace/build contract changed (rebuild
/// everything); otherwise `packages` is the reverse-dependency closure of the
/// changed packages. Empty and not full means nothing to do.
pub struct Plan {
    pub full: bool,
    pub packages: BTreeSet<String>,
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

/// The affected-package plan for a set of changed files.
pub fn plan(workspace: &Path, files: &[String]) -> Result<Plan> {
    let empty = Plan {
        full: false,
        packages: BTreeSet::new(),
    };
    if files.is_empty() {
        return Ok(empty);
    }
    if files.iter().any(|f| is_build_contract(f)) {
        return Ok(Plan {
            full: true,
            packages: BTreeSet::new(),
        });
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
    let mut reverse: HashMap<String, HashSet<String>> = HashMap::new();
    for p in &pkgs {
        for dep in &p.dependencies {
            if names.contains(dep.name.as_str()) {
                reverse
                    .entry(dep.name.clone())
                    .or_default()
                    .insert(p.name.clone());
            }
        }
    }

    let mut changed: HashSet<String> = HashSet::new();
    for file in files {
        if let Some(name) = package_for_file(file, &dirs) {
            changed.insert(name.to_string());
        }
    }
    Ok(Plan {
        full: false,
        packages: reverse_closure(changed, &reverse),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_contracts_force_a_full_rebuild() {
        for f in [
            "Cargo.toml",
            "Cargo.lock",
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
}
