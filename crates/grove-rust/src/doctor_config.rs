use super::SettingSource;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "doctor_config_home.rs"]
mod cargo_home;
#[path = "doctor_config_portable.rs"]
mod portable;
#[path = "doctor_config_profiles.rs"]
mod profile_set;

pub(super) struct Document {
    pub(super) source: String,
    pub(super) value: toml::Value,
    raw: Vec<u8>,
    resolved: PathBuf,
    repository: bool,
}

pub(super) struct Inputs {
    pub(super) manifest: Document,
    pub(super) configs: Vec<Document>,
}

#[derive(Default)]
struct Traversal {
    active: BTreeSet<PathBuf>,
    loaded: BTreeSet<PathBuf>,
    documents: Vec<Document>,
}

#[derive(Clone)]
pub(super) struct Resolved {
    pub(super) value: toml::Value,
    pub(super) source: SettingSource,
}

pub(super) fn load(workspace: &Path) -> Result<Inputs> {
    let manifest = document(&workspace.join("Cargo.toml"), "Cargo.toml", true)?;
    Ok(Inputs {
        manifest,
        configs: configs(workspace)?,
    })
}

/// Portable verification excludes Cargo inputs that escape the captured repository.
pub(crate) fn portable_supported(workspace: &Path) -> Result<bool> {
    portable::supported(workspace)
}

pub(crate) fn config_inputs(workspace: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    Ok(configs(workspace)?
        .into_iter()
        .map(|document| (document.source, document.raw))
        .collect())
}

pub(super) fn repository_configs(inputs: &Inputs) -> impl Iterator<Item = &Document> {
    inputs.configs.iter().filter(|document| document.repository)
}

pub(super) fn profiles(inputs: &Inputs) -> BTreeSet<String> {
    profile_set::names(inputs)
}

pub(super) fn setting(inputs: &Inputs, profile: &str, key: &str) -> Option<Resolved> {
    if key == "incremental"
        && let Some(value) = build_incremental(inputs)
    {
        return Some(value);
    }
    let mut seen = BTreeSet::new();
    resolve(inputs, profile, key, &mut seen)
}

pub(super) fn identity(inputs: &Inputs) -> String {
    let mut hash = Sha256::new();
    hash.update(b"grove.incremental-policy.v2\0");
    profile_table(&mut hash, &inputs.manifest);
    for document in &inputs.configs {
        text(&mut hash, &document.source);
        bytes(&mut hash, document.resolved.as_os_str().as_encoded_bytes());
        bytes(&mut hash, &document.raw);
    }
    let mut variables = BTreeSet::from([
        "CARGO_INCREMENTAL".to_string(),
        "CARGO_BUILD_INCREMENTAL".to_string(),
    ]);
    variables.extend(env::vars_os().filter_map(|(name, _)| {
        let name = name.to_string_lossy();
        (name.starts_with("CARGO_PROFILE_") && name.ends_with("_INCREMENTAL"))
            .then(|| name.into_owned())
    }));
    for name in variables {
        text(&mut hash, &name);
        match env::var_os(&name) {
            Some(value) => {
                hash.update([1]);
                bytes(&mut hash, value.as_encoded_bytes());
            }
            None => hash.update([0]),
        }
    }
    format!("{:x}", hash.finalize())
}

fn configs(workspace: &Path) -> Result<Vec<Document>> {
    let mut traversal = Traversal::default();
    let repository = fs::canonicalize(workspace)
        .with_context(|| format!("resolving {}", workspace.display()))?;
    for (distance, directory) in workspace.ancestors().enumerate() {
        let Some(path) = config_path(directory) else {
            continue;
        };
        let source = if distance == 0 {
            let relative = path
                .strip_prefix(workspace)
                .context("repository Cargo config is outside the workspace")?;
            grove_core::scope::normalize(&relative.to_string_lossy())?
        } else {
            format!("ancestor-{distance}/.cargo/{}", file_name(&path)?)
        };
        load_config(&path, source, &repository, &mut traversal)?;
    }
    let home = cargo_home::path(workspace, env::var_os("CARGO_HOME"));
    if let Some(path) = cargo_home_path(&home) {
        load_config(
            &path,
            format!("cargo-home/{}", file_name(&path)?),
            &repository,
            &mut traversal,
        )?;
    }
    Ok(traversal.documents)
}

fn config_path(directory: &Path) -> Option<PathBuf> {
    let legacy = directory.join(".cargo").join("config");
    if legacy.is_file() {
        return Some(legacy);
    }
    let modern = directory.join(".cargo").join("config.toml");
    modern.is_file().then_some(modern)
}

fn cargo_home_path(directory: &Path) -> Option<PathBuf> {
    let legacy = directory.join("config");
    if legacy.is_file() {
        return Some(legacy);
    }
    let modern = directory.join("config.toml");
    modern.is_file().then_some(modern)
}

fn load_config(
    path: &Path,
    source: String,
    repository: &Path,
    traversal: &mut Traversal,
) -> Result<()> {
    let mut document = document(path, &source, false)?;
    if traversal.loaded.contains(&document.resolved) {
        return Ok(());
    }
    if !traversal.active.insert(document.resolved.clone()) {
        bail!("Cargo config include cycle at {}", path.display())
    }
    let resolved = document.resolved.clone();
    document.repository = document.resolved.starts_with(repository);
    let includes = includes(&document.value, path)?;
    traversal.documents.push(document);
    for (index, include) in includes.into_iter().enumerate().rev() {
        let source = format!("{source}.include-{index}");
        match include {
            Include::Required(path) => load_config(&path, source, repository, traversal)?,
            Include::Optional(path) if path.exists() => {
                load_config(&path, source, repository, traversal)?
            }
            Include::Optional(_) => {}
        }
    }
    traversal.active.remove(&resolved);
    traversal.loaded.insert(resolved);
    Ok(())
}

enum Include {
    Required(PathBuf),
    Optional(PathBuf),
}

fn includes(value: &toml::Value, path: &Path) -> Result<Vec<Include>> {
    let Some(value) = value.get("include") else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .context("Cargo config include must be an array")?;
    values
        .iter()
        .map(|value| {
            let (value, optional) = match value {
                toml::Value::String(path) => (path.as_str(), false),
                toml::Value::Table(table) => (
                    table
                        .get("path")
                        .and_then(toml::Value::as_str)
                        .context("Cargo config include needs a string path")?,
                    table.get("optional").map_or(Ok(false), |value| {
                        value
                            .as_bool()
                            .context("Cargo config include optional must be a boolean")
                    })?,
                ),
                _ => bail!("Cargo config include entries must be strings or tables"),
            };
            let value = Path::new(value);
            if value
                .extension()
                .is_none_or(|extension| extension != "toml")
            {
                bail!("Cargo config includes must name .toml files")
            }
            let path = if value.is_absolute() {
                value.to_path_buf()
            } else {
                path.parent()
                    .context("Cargo config has no parent")?
                    .join(value)
            };
            Ok(if optional {
                Include::Optional(path)
            } else {
                Include::Required(path)
            })
        })
        .collect()
}

fn document(path: &Path, source: &str, repository: bool) -> Result<Document> {
    let resolved =
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = std::str::from_utf8(&raw).with_context(|| format!("reading {}", path.display()))?;
    let value = toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Document {
        source: source.to_string(),
        value,
        raw,
        resolved,
        repository,
    })
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .context("Cargo config has no UTF-8 file name")
}

fn resolve(
    inputs: &Inputs,
    profile: &str,
    key: &str,
    seen: &mut BTreeSet<String>,
) -> Option<Resolved> {
    if !seen.insert(profile.to_string()) {
        return None;
    }
    let resolved = profile_env(profile, key)
        .or_else(|| {
            inputs
                .configs
                .iter()
                .find_map(|document| profile_value(document, profile, key))
        })
        .or_else(|| profile_value(&inputs.manifest, profile, key))
        .or_else(|| parent(inputs, profile).and_then(|parent| resolve(inputs, &parent, key, seen)))
        .or_else(|| default(profile, key));
    seen.remove(profile);
    resolved
}

fn build_incremental(inputs: &Inputs) -> Option<Resolved> {
    env_value("CARGO_INCREMENTAL", "environment")
        .or_else(|| env_value("CARGO_BUILD_INCREMENTAL", "environment"))
        .or_else(|| {
            inputs.configs.iter().find_map(|document| {
                document
                    .value
                    .get("build")?
                    .get("incremental")
                    .cloned()
                    .map(|value| Resolved {
                        value,
                        source: SettingSource {
                            source: document.source.clone(),
                            key: "build.incremental".to_string(),
                        },
                    })
            })
        })
}

fn profile_env(profile: &str, key: &str) -> Option<Resolved> {
    let profile = profile.replace('-', "_").to_ascii_uppercase();
    let key = key.replace('-', "_").to_ascii_uppercase();
    let name = format!("CARGO_PROFILE_{profile}_{key}");
    env_value(&name, "environment")
}

fn env_value(name: &str, source: &str) -> Option<Resolved> {
    let value = env::var(name).ok()?;
    let value = match value.as_str() {
        "0" | "false" => toml::Value::Boolean(false),
        "1" | "true" => toml::Value::Boolean(true),
        _ => toml::Value::String(value),
    };
    Some(Resolved {
        value,
        source: SettingSource {
            source: source.to_string(),
            key: name.to_string(),
        },
    })
}

fn profile_value(document: &Document, profile: &str, key: &str) -> Option<Resolved> {
    document
        .value
        .get("profile")?
        .get(profile)?
        .get(key)
        .cloned()
        .map(|value| Resolved {
            value,
            source: SettingSource {
                source: document.source.clone(),
                key: format!("profile.{profile}.{key}"),
            },
        })
}

fn parent(inputs: &Inputs, profile: &str) -> Option<String> {
    match profile {
        "test" => Some("dev".to_string()),
        "bench" => Some("release".to_string()),
        _ => inputs
            .configs
            .iter()
            .chain(std::iter::once(&inputs.manifest))
            .find_map(|document| {
                document
                    .value
                    .get("profile")?
                    .get(profile)?
                    .get("inherits")?
                    .as_str()
                    .map(str::to_string)
            }),
    }
}

fn default(profile: &str, key: &str) -> Option<Resolved> {
    let value = match (profile, key) {
        ("dev", "opt-level") => toml::Value::Integer(0),
        ("dev", "incremental") => toml::Value::Boolean(true),
        ("release", "opt-level") | ("bench", "opt-level") => toml::Value::Integer(3),
        ("release", "incremental") | ("bench", "incremental") => toml::Value::Boolean(false),
        _ => return None,
    };
    Some(Resolved {
        value,
        source: SettingSource {
            source: "Cargo default".to_string(),
            key: format!("profile.{profile}.{key}"),
        },
    })
}

fn profile_table(hash: &mut Sha256, document: &Document) {
    text(hash, &document.source);
    match document.value.get("profile") {
        Some(value) => bytes(hash, value.to_string().as_bytes()),
        None => hash.update([0]),
    }
}

fn text(hash: &mut Sha256, value: &str) {
    bytes(hash, value.as_bytes());
}

fn bytes(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}
