use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "materialization_cargo_path.rs"]
mod paths;
use paths::{file_url, logical, repo_id, repo_path, text};

pub struct Fingerprint {
    pub hash: String,
    pub value: Value,
}
type Roots = [String; 2];
pub fn capture(workspace: &Path, repo_root: &Path) -> Result<Fingerprint> {
    let workspace = fs::canonicalize(workspace).context("canonicalizing Cargo directory")?;
    let repo = fs::canonicalize(repo_root).context("canonicalizing Git repository root")?;
    if !workspace.starts_with(&repo) {
        bail!("Cargo working directory is outside the Git repository")
    }
    let mut cargo = metadata(&workspace)?;
    let root = cargo
        .get("workspace_root")
        .and_then(Value::as_str)
        .context("Cargo metadata omitted workspace_root")?;
    let inputs = tracked_inputs(&workspace, &repo, Path::new(root))?;
    let roots = [text(&repo)?, file_url(&repo)?];
    normalize(&mut cargo, &roots)?;
    let mut value = json!({"cargo": cargo, "tracked_inputs": inputs});
    canonicalize(&mut value, &mut Vec::new());
    let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?));
    Ok(Fingerprint { hash, value })
}

pub fn equivalent(source: &Fingerprint, candidate: &Fingerprint) -> Result<bool> {
    if source.value == candidate.value {
        return Ok(true);
    }
    if ambiguous(&source.value, &candidate.value) {
        bail!("unknown path-shaped metadata difference")
    }
    Ok(false)
}

fn metadata(workspace: &Path) -> Result<Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(workspace)
        .output()
        .context("spawning cargo metadata")?;
    if !output.status.success() {
        bail!("cargo metadata failed")
    }
    serde_json::from_slice(&output.stdout).context("parsing Cargo metadata format 1")
}

fn normalize(cargo: &mut Value, roots: &Roots) -> Result<()> {
    rooted_field(cargo, "workspace_root", roots, false)?;
    for (key, variable, token) in [
        ("target_directory", "CARGO_TARGET_DIR", "$GROVE_TARGET"),
        ("build_directory", "CARGO_BUILD_BUILD_DIR", "$GROVE_BUILD"),
    ] {
        owned_field(cargo, key, variable, token, roots)?;
    }
    for package in array(cargo, "packages")? {
        rooted_field(package, "id", roots, true)?;
        rooted_field(package, "manifest_path", roots, false)?;
        rooted_field(package, "license_file", roots, false)?;
        rooted_field(package, "readme", roots, false)?;
        for dependency in array(package, "dependencies")? {
            rooted_field(dependency, "path", roots, false)?;
        }
        for target in array(package, "targets")? {
            rooted_field(target, "src_path", roots, false)?;
        }
    }
    for key in ["workspace_members", "workspace_default_members"] {
        ids(cargo, key, roots)?;
    }
    if let Some(resolve) = cargo.get_mut("resolve").filter(|value| !value.is_null()) {
        normalize_resolve(resolve, roots)?;
    }
    Ok(())
}

fn normalize_resolve(resolve: &mut Value, roots: &Roots) -> Result<()> {
    rooted_field(resolve, "root", roots, true)?;
    for node in array(resolve, "nodes")? {
        rooted_field(node, "id", roots, true)?;
        ids(node, "dependencies", roots)?;
        for dependency in array(node, "deps")? {
            rooted_field(dependency, "pkg", roots, true)?;
        }
    }
    Ok(())
}

fn owned_field(
    value: &mut Value,
    key: &str,
    variable: &str,
    token: &str,
    roots: &Roots,
) -> Result<()> {
    match value.get_mut(key) {
        None | Some(Value::Null) => {}
        Some(Value::String(path)) => {
            let owned = std::env::var_os(variable)
                .and_then(|root| root.into_string().ok())
                .is_some_and(|root| logical(&root) == logical(path));
            *path = if owned {
                token.into()
            } else {
                repo_path(path, roots)
            };
        }
        Some(_) => bail!("Cargo metadata field {key:?} is not a string"),
    }
    Ok(())
}

fn rooted_field(value: &mut Value, key: &str, roots: &Roots, id: bool) -> Result<()> {
    let Some(field) = value.get_mut(key) else {
        return Ok(());
    };
    if field.is_null() {
        return Ok(());
    }
    let text = field
        .as_str()
        .with_context(|| format!("Cargo metadata field {key:?} is not a string"))?;
    *field = Value::String(if id {
        repo_id(text, roots)
    } else {
        repo_path(text, roots)
    });
    Ok(())
}

fn ids(value: &mut Value, key: &str, roots: &Roots) -> Result<()> {
    let Some(values) = value.get_mut(key) else {
        return Ok(());
    };
    for value in values
        .as_array_mut()
        .with_context(|| format!("Cargo metadata field {key:?} is not an array"))?
    {
        let id = value
            .as_str()
            .with_context(|| format!("Cargo metadata field {key:?} contains a non-string"))?;
        *value = Value::String(repo_id(id, roots));
    }
    Ok(())
}

fn tracked_inputs(workspace: &Path, repo: &Path, root: &Path) -> Result<Vec<Value>> {
    let root = fs::canonicalize(root).context("canonicalizing Cargo workspace root")?;
    if !root.starts_with(repo) {
        bail!("Cargo workspace root is outside the Git repository")
    }
    let mut paths = BTreeSet::new();
    for name in ["Cargo.toml", "Cargo.lock"] {
        present(&mut paths, root.join(name))?;
    }
    present(&mut paths, repo.join(".grove.toml"))?;
    ancestors(workspace, repo, &mut paths)?;
    included(&mut paths)?;
    let tracked = tracked(repo)?;
    paths
        .into_iter()
        .map(|path| input(repo, &tracked, &path))
        .collect()
}

fn ancestors(workspace: &Path, repo: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut dir = workspace.to_path_buf();
    loop {
        for name in [
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            present(paths, dir.join(name))?;
        }
        if dir == repo {
            return Ok(());
        }
        dir = dir
            .parent()
            .filter(|parent| parent.starts_with(repo))
            .context("Cargo working directory is outside the Git repository")?
            .into();
    }
}

fn included(paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut pending: Vec<_> = paths
        .iter()
        .filter(|path| path.ends_with(".cargo/config") || path.ends_with(".cargo/config.toml"))
        .cloned()
        .collect();
    while let Some(config) = pending.pop() {
        let value: toml::Value = fs::read_to_string(&config)
            .with_context(|| format!("reading Cargo config {}", config.display()))?
            .parse()
            .with_context(|| format!("parsing Cargo config {}", config.display()))?;
        for (path, optional) in includes(&value)? {
            let path = config
                .parent()
                .context("Cargo config has no parent directory")?
                .join(path);
            if optional && !path.try_exists()? {
                continue;
            }
            if paths.insert(path.clone()) {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn includes(value: &toml::Value) -> Result<Vec<(String, bool)>> {
    let Some(items) = value.get("include") else {
        return Ok(Vec::new());
    };
    items
        .as_array()
        .context("Cargo config include must be an array")?
        .iter()
        .map(include)
        .collect()
}

fn include(item: &toml::Value) -> Result<(String, bool)> {
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
    let optional = table
        .get("optional")
        .map(|value| {
            value
                .as_bool()
                .context("Cargo config include optional must be a boolean")
        })
        .transpose()?
        .unwrap_or(false);
    Ok((path.into(), optional))
}

fn input(repo: &Path, tracked: &BTreeSet<String>, path: &Path) -> Result<Value> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("canonicalizing Cargo input {}", path.display()))?;
    let relative = text(
        path.strip_prefix(repo)
            .with_context(|| format!("Cargo input {} is outside repository", path.display()))?,
    )?;
    if !tracked.contains(&relative) {
        bail!("Cargo input {relative:?} is not tracked")
    }
    let bytes =
        fs::read(&path).with_context(|| format!("reading Cargo input {}", path.display()))?;
    Ok(json!({"path": relative, "sha256": format!("{:x}", Sha256::digest(bytes))}))
}

#[rustfmt::skip]
fn present(paths: &mut BTreeSet<PathBuf>, path: PathBuf) -> Result<()> { if path.try_exists()? { paths.insert(path); } Ok(()) }

fn tracked(repo: &Path) -> Result<BTreeSet<String>> {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo)
        .output()
        .context("spawning git ls-files for Cargo fingerprint")?;
    if !output.status.success() {
        bail!("git ls-files failed while fingerprinting Cargo inputs")
    }
    String::from_utf8(output.stdout)
        .context("tracked Git paths are not UTF-8")
        .map(|paths| paths.split_terminator('\0').map(str::to_owned).collect())
}

fn array<'a>(value: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>> {
    value
        .get_mut(key)
        .with_context(|| format!("Cargo metadata omitted {key:?}"))?
        .as_array_mut()
        .with_context(|| format!("Cargo metadata field {key:?} is not an array"))
}

fn canonicalize(value: &mut Value, path: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                path.push(key.clone());
                canonicalize(value, path);
                path.pop();
            }
            map.sort_keys();
        }
        Value::Array(values) => {
            for value in values.iter_mut() {
                path.push("*".into());
                canonicalize(value, path);
                path.pop();
            }
            if unordered(path) {
                values.sort_by_cached_key(Value::to_string);
            }
        }
        _ => {}
    }
}

fn unordered(path: &[String]) -> bool {
    let path = path.join("/");
    UNORDERED.contains(&path.as_str()) || path.starts_with("cargo/packages/*/features/")
}

#[rustfmt::skip]
const UNORDERED: &[&str] = &[
    "cargo/packages", "cargo/workspace_members", "cargo/workspace_default_members",
    "cargo/packages/*/dependencies", "cargo/packages/*/targets", "cargo/resolve/nodes",
    "cargo/resolve/nodes/*/dependencies", "cargo/resolve/nodes/*/deps", "cargo/resolve/nodes/*/features",
    "cargo/packages/*/dependencies/*/features", "cargo/packages/*/targets/*/kind",
    "cargo/packages/*/targets/*/crate_types", "cargo/packages/*/targets/*/required-features",
    "cargo/resolve/nodes/*/deps/*/dep_kinds",
];

fn ambiguous(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(left), Value::String(right)) if left != right => {
            path_string(left) || path_string(right)
        }
        (Value::Array(left), Value::Array(right)) => {
            left.iter().zip(right).any(|(a, b)| ambiguous(a, b))
                || left.len() != right.len() && left.iter().chain(right).any(contains_path)
        }
        (Value::Object(left), Value::Object(right)) => {
            left.keys()
                .chain(right.keys())
                .any(|key| match (left.get(key), right.get(key)) {
                    (Some(a), Some(b)) => ambiguous(a, b),
                    (Some(value), None) | (None, Some(value)) => contains_path(value),
                    (None, None) => false,
                })
        }
        _ => false,
    }
}

fn contains_path(value: &Value) -> bool {
    match value {
        Value::String(value) => path_string(value),
        Value::Array(values) => values.iter().any(contains_path),
        Value::Object(values) => values.values().any(contains_path),
        _ => false,
    }
}

#[rustfmt::skip]
fn path_string(value: &str) -> bool { Path::new(value).is_absolute() || value.starts_with("file:") || value.starts_with("path+file:") }

#[cfg(test)]
include!("materialization_cargo_tests.rs");
