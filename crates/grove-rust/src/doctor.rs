//! Read-only diagnostics for build acceleration choices Grove must not impose.

use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

#[path = "doctor_config.rs"]
mod cargo_config;
#[path = "doctor_watchlist.rs"]
mod watchlist;

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub mold: Mold,
    pub incremental: Incremental,
    pub watchlist: Vec<Watch>,
}

#[derive(Serialize)]
pub struct Mold {
    pub supported: bool,
    pub available: bool,
    pub version: Option<String>,
    pub linker_settings: Vec<LinkerSetting>,
    pub repository_default_linker: bool,
    pub limitation: String,
}

#[derive(Serialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinkerSetting {
    pub source: String,
    pub scope: String,
    pub linker: String,
}

#[derive(Serialize)]
pub struct Incremental {
    pub identity_sha256: String,
    pub disabled_profiles: Vec<IncrementalProfile>,
    pub limitation: String,
}

#[derive(Serialize)]
pub struct IncrementalProfile {
    pub profile: String,
    pub opt_level: String,
    pub incremental_source: SettingSource,
    pub opt_level_source: SettingSource,
}

#[derive(Serialize, Clone)]
pub struct SettingSource {
    pub source: String,
    pub key: String,
}

#[derive(Serialize)]
pub struct Watch {
    pub id: String,
    pub status: String,
    pub detail: String,
}

pub fn report(workspace: &Path) -> Result<Report> {
    let inputs = cargo_config::load(workspace)?;
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        mold: mold(cargo_config::repository_configs(&inputs)),
        incremental: incremental(&inputs),
        watchlist: watchlist::items(),
    })
}

/// Hash every Cargo input that can alter whether a lane's incremental artifacts are
/// compatible. This intentionally binds broader Cargo profile/config context than the
/// human-readable report, because cache reuse must fail closed.
pub(crate) fn incremental_identity(workspace: &Path) -> Result<String> {
    Ok(cargo_config::identity(&cargo_config::load(workspace)?))
}

/// Raw, recursively-expanded Cargo config inputs used by portable verification receipts.
pub(crate) fn cargo_config_inputs(workspace: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    cargo_config::config_inputs(workspace)
}

/// Whether Cargo configuration stays inside the portable receipt contract.
pub(crate) fn portable_cargo_config_supported(workspace: &Path) -> Result<bool> {
    cargo_config::portable_supported(workspace)
}

fn mold<'a>(configs: impl Iterator<Item = &'a cargo_config::Document>) -> Mold {
    let linker_settings = configs
        .flat_map(linker_settings)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !cfg!(target_os = "linux") {
        return Mold {
            supported: false,
            available: false,
            version: None,
            repository_default_linker: linker_settings.is_empty(),
            linker_settings,
            limitation: "mold is reported as a Linux-only candidate; local settings cannot prove the linker used by a build".to_string(),
        };
    }
    let output = Command::new("mold").arg("--version").output().ok();
    let version = output.as_ref().and_then(|output| {
        output.status.success().then(|| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        })
    });
    Mold {
        supported: true,
        available: version.is_some(),
        version,
        repository_default_linker: linker_settings.is_empty(),
        linker_settings,
        limitation: "this only reports repository-local Cargo config; environment, user config, wrappers, and artifacts may select another linker".to_string(),
    }
}

fn linker_settings(config: &cargo_config::Document) -> Vec<LinkerSetting> {
    let mut settings = BTreeSet::new();
    if let Some(build) = config.value.get("build")
        && let Some(flags) = build.get("rustflags")
    {
        insert_flag_linkers(&mut settings, config, "build.rustflags", flags);
    }
    let Some(targets) = config.value.get("target").and_then(toml::Value::as_table) else {
        return settings.into_iter().collect();
    };
    for (target, value) in targets {
        if let Some(linker) = value.get("linker").and_then(toml::Value::as_str) {
            settings.insert(LinkerSetting {
                source: config.source.clone(),
                scope: format!("target.{target}.linker"),
                linker: linker.to_string(),
            });
        }
        if let Some(flags) = value.get("rustflags") {
            insert_flag_linkers(
                &mut settings,
                config,
                &format!("target.{target}.rustflags"),
                flags,
            );
        }
    }
    settings.into_iter().collect()
}

fn insert_flag_linkers(
    settings: &mut BTreeSet<LinkerSetting>,
    config: &cargo_config::Document,
    scope: &str,
    value: &toml::Value,
) {
    for linker in flag_linkers(value) {
        settings.insert(LinkerSetting {
            source: config.source.clone(),
            scope: scope.to_string(),
            linker,
        });
    }
}

fn flag_linkers(value: &toml::Value) -> Vec<String> {
    let flags: Vec<String> = match value {
        toml::Value::String(flags) => flags.split_whitespace().map(str::to_string).collect(),
        toml::Value::Array(flags) => flags
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let mut linkers = Vec::new();
    for (index, flag) in flags.iter().enumerate() {
        let linker = flag.strip_prefix("-Clinker=").or_else(|| {
            (flag == "-C")
                .then(|| flags.get(index + 1))
                .flatten()
                .and_then(|next| next.strip_prefix("linker="))
        });
        if let Some(linker) = linker.filter(|linker| !linker.is_empty()) {
            linkers.push(linker.to_string());
        }
    }
    linkers
}

fn incremental(inputs: &cargo_config::Inputs) -> Incremental {
    let mut disabled_profiles = Vec::new();
    for profile in cargo_config::profiles(inputs) {
        let Some(opt_level) = cargo_config::setting(inputs, &profile, "opt-level") else {
            continue;
        };
        let Some(incremental) = cargo_config::setting(inputs, &profile, "incremental") else {
            continue;
        };
        if incremental.value.as_bool() == Some(false) && optimized(&opt_level.value) {
            let opt = opt_level
                .value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| opt_level.value.to_string());
            disabled_profiles.push(IncrementalProfile {
                profile,
                opt_level: opt,
                incremental_source: incremental.source,
                opt_level_source: opt_level.source,
            });
        }
    }
    Incremental {
        identity_sha256: cargo_config::identity(inputs),
        disabled_profiles,
        limitation: "reports top-level effective profiles and their Cargo config/environment precedence; package and build overrides are bound into lane identity but are not expanded as separate profile rows".to_string(),
    }
}

fn optimized(value: &toml::Value) -> bool {
    value.as_integer().is_some_and(|level| level > 0)
        || value.as_str().is_some_and(|level| level != "0")
}
