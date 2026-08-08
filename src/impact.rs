//! Git diff → workspace package names for `cargo -p`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Plan {
    /// Workspace-wide check (lockfile / toolchain / cargo config touched).
    pub full: bool,
    /// Package names for `cargo -p` (empty when `full` or nothing relevant changed).
    pub packages: Vec<String>,
    pub changed_files: Vec<String>,
    /// Diff base actually used (after resolving `auto`).
    pub base: String,
}

fn porcelain_paths(workspace: &Path) -> Result<Vec<String>> {
    let out = crate::git::stdout(workspace, &["status", "--porcelain", "-z"])?;
    Ok(parse_porcelain_z(&out))
}

fn parse_porcelain_z(s: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut parts = s.split('\0').filter(|p| !p.is_empty());
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let xy = &entry[..2];
        let path_part = entry[3..].trim();
        if xy.contains('R') || xy.contains('C') {
            if !path_part.is_empty() {
                let old = path_part.split('\t').next_back().unwrap_or(path_part);
                paths.push(old.replace('\\', "/"));
            }
            if let Some(new_path) = parts.next() {
                paths.push(new_path.replace('\\', "/"));
            }
        } else if !path_part.is_empty() {
            paths.push(path_part.replace('\\', "/"));
        }
    }
    paths
}

fn ref_exists(workspace: &Path, rev: &str) -> bool {
    crate::git::ok(workspace, &["rev-parse", "--verify", "--quiet", rev])
}

fn resolve_base_cached(workspace: &Path, base: &str) -> String {
    if base != "auto" {
        return base.to_string();
    }
    static CACHE: Mutex<Option<(PathBuf, String)>> = Mutex::new(None);
    let canon = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    if let Ok(guard) = CACHE.lock()
        && let Some((p, b)) = guard.as_ref()
        && *p == canon
    {
        return b.clone();
    }
    let resolved = resolve_base_uncached(workspace);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((canon, resolved.clone()));
    }
    resolved
}

fn resolve_base_uncached(workspace: &Path) -> String {
    for c in ["origin/main", "main", "origin/master", "master"] {
        if ref_exists(workspace, c) {
            return c.to_string();
        }
    }
    "HEAD".into()
}

/// Resolve `auto` to origin/main, main, origin/master, master, or HEAD.
pub fn resolve_base(workspace: &Path, base: &str) -> String {
    if base != "auto" {
        return base.to_string();
    }
    resolve_base_cached(workspace, "auto")
}

/// Paths changed since merge-base(base, HEAD), plus unstaged and untracked.
pub fn changed_files(workspace: &Path, base: &str) -> Result<Vec<String>> {
    let base = resolve_base(workspace, base);
    let mut files: BTreeSet<String> = porcelain_paths(workspace)?.into_iter().collect();

    if files.is_empty() && base == "HEAD" {
        return Ok(vec![]);
    }

    if base != "HEAD" {
        let head = crate::git::stdout(workspace, &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        let merge_base = crate::git::stdout(workspace, &["merge-base", &base, "HEAD"])?
            .trim()
            .to_string();
        if merge_base != head {
            let out = crate::git::stdout(
                workspace,
                &["diff", "--name-only", "-z", &merge_base, "HEAD"],
            )?;
            for f in out.split('\0').filter(|s| !s.is_empty()) {
                files.insert(f.replace('\\', "/"));
            }
        }
    }
    Ok(files.into_iter().collect())
}

fn is_build_contract(file: &str) -> bool {
    matches!(
        file,
        "Cargo.toml"
            | "Cargo.lock"
            | "rust-toolchain"
            | "rust-toolchain.toml"
            | ".cargo/config"
            | ".cargo/config.toml"
    ) || file.starts_with(".cargo/")
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: Vec<String>,
    resolve: Option<Resolve>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct MetaPackage {
    name: String,
    id: String,
    manifest_path: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Resolve {
    nodes: Vec<ResolveNode>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ResolveNode {
    id: String,
    dependencies: Vec<String>,
}

fn metadata(workspace: &Path) -> Result<Metadata> {
    use std::time::SystemTime;
    static CACHE: Mutex<Option<(PathBuf, Option<SystemTime>, Metadata)>> = Mutex::new(None);

    let canon = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let lock_m = workspace
        .join("Cargo.lock")
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok());
    if let Ok(guard) = CACHE.lock()
        && let Some((p, m, meta)) = guard.as_ref()
        && *p == canon
        && *m == lock_m
    {
        return Ok(meta.clone());
    }

    let meta: Metadata = serde_json::from_slice(&crate::util::cargo_metadata_bytes(workspace)?)
        .context("parse cargo metadata")?;
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((canon, lock_m, meta.clone()));
    }
    Ok(meta)
}

fn package_for_abs_file(file: &Path, package_dirs: &[(String, PathBuf)]) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for (name, dir) in package_dirs {
        if file.starts_with(dir) {
            let n = dir.as_os_str().len();
            if best.as_ref().map(|(l, _)| n > *l).unwrap_or(true) {
                best = Some((n, name.clone()));
            }
        }
    }
    best.map(|(_, name)| name)
}

fn workspace_package_names(meta: &Metadata) -> HashSet<String> {
    let members: HashSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
    meta.packages
        .iter()
        .filter(|p| members.contains(p.id.as_str()))
        .map(|p| p.name.clone())
        .collect()
}

fn reverse_deps(
    meta: &Metadata,
    workspace_names: &HashSet<String>,
) -> HashMap<String, HashSet<String>> {
    let id_to_name: HashMap<_, _> = meta
        .packages
        .iter()
        .filter(|p| workspace_names.contains(&p.name))
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    let mut rev: HashMap<String, HashSet<String>> = HashMap::new();
    let Some(resolve) = &meta.resolve else {
        return rev;
    };
    for node in &resolve.nodes {
        let Some(from) = id_to_name.get(&node.id) else {
            continue;
        };
        for dep_id in &node.dependencies {
            if let Some(to) = id_to_name.get(dep_id) {
                rev.entry(to.clone()).or_default().insert(from.clone());
            }
        }
    }
    rev
}

fn closure(seeds: &HashSet<String>, rev: &HashMap<String, HashSet<String>>) -> Vec<String> {
    let mut out: BTreeSet<String> = seeds.iter().cloned().collect();
    let mut stack: Vec<String> = seeds.iter().cloned().collect();
    while let Some(p) = stack.pop() {
        if let Some(dependents) = rev.get(&p) {
            for d in dependents {
                if out.insert(d.clone()) {
                    stack.push(d.clone());
                }
            }
        }
    }
    out.into_iter().collect()
}

pub fn plan_from_files(workspace: &Path, files: &[String], base: &str) -> Result<Plan> {
    if files.is_empty() {
        return Ok(Plan {
            full: false,
            packages: vec![],
            changed_files: vec![],
            base: base.to_string(),
        });
    }
    if files.iter().any(|f| is_build_contract(f)) {
        return Ok(Plan {
            full: true,
            packages: vec![],
            changed_files: files.to_vec(),
            base: base.to_string(),
        });
    }

    let meta = metadata(workspace)?;
    let workspace_names = workspace_package_names(&meta);
    let ws = crate::git::stdout(
        workspace,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    )
    .map(|s| PathBuf::from(s.trim()))
    .unwrap_or_else(|_| {
        std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
    });
    let package_dirs: Vec<(String, PathBuf)> = meta
        .packages
        .iter()
        .filter(|p| workspace_names.contains(&p.name))
        .filter_map(|p| {
            let manifest = PathBuf::from(&p.manifest_path);
            let dir = manifest.parent()?.to_path_buf();
            Some((p.name.clone(), dir))
        })
        .collect();

    let mut seeds = HashSet::new();
    for f in files {
        let abs = ws.join(f);
        if let Some(pkg) = package_for_abs_file(&abs, &package_dirs) {
            seeds.insert(pkg);
        }
    }

    if seeds.is_empty() {
        return Ok(Plan {
            full: false,
            packages: vec![],
            changed_files: files.to_vec(),
            base: base.to_string(),
        });
    }

    let rev = reverse_deps(&meta, &workspace_names);
    Ok(Plan {
        full: false,
        packages: closure(&seeds, &rev),
        changed_files: files.to_vec(),
        base: base.to_string(),
    })
}

pub fn plan(workspace: &Path, base: &str) -> Result<Plan> {
    let base = resolve_base(workspace, base);
    if base == "HEAD" {
        let dirty = porcelain_paths(workspace)?;
        if dirty.is_empty() {
            return Ok(Plan {
                full: false,
                packages: vec![],
                changed_files: vec![],
                base,
            });
        }
        return plan_from_files(workspace, &dirty, &base);
    }

    let files = changed_files(workspace, &base)?;
    if files.is_empty() {
        return Ok(Plan {
            full: false,
            packages: vec![],
            changed_files: vec![],
            base,
        });
    }
    plan_from_files(workspace, &files, &base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_contract_paths() {
        assert!(is_build_contract("Cargo.toml"));
        assert!(is_build_contract("Cargo.lock"));
        assert!(is_build_contract(".cargo/config.toml"));
        assert!(!is_build_contract("src/main.rs"));
        assert!(!is_build_contract("crates/foo/Cargo.toml"));
    }

    #[test]
    fn package_prefix_prefers_longest() {
        let dirs = vec![
            ("root".into(), PathBuf::from("/ws")),
            ("nested".into(), PathBuf::from("/ws/crates/nested")),
        ];
        assert_eq!(
            package_for_abs_file(Path::new("/ws/crates/nested/src/lib.rs"), &dirs).as_deref(),
            Some("nested")
        );
    }

    #[test]
    fn resolve_base_passthrough() {
        assert_eq!(resolve_base(Path::new("."), "HEAD"), "HEAD");
        assert_eq!(resolve_base(Path::new("."), "origin/main"), "origin/main");
    }

    #[test]
    fn porcelain_parse_ordinary() {
        let p = parse_porcelain_z(" M src/lib.rs\0?? new.rs\0");
        assert!(p.iter().any(|x| x.contains("lib.rs")));
        assert!(p.iter().any(|x| x.contains("new.rs")));
    }

    #[test]
    fn porcelain_parse_rename_z() {
        let p = parse_porcelain_z("R  old.rs\0new.rs\0");
        assert!(p.iter().any(|x| x == "old.rs"));
        assert!(p.iter().any(|x| x == "new.rs"));
    }
}
