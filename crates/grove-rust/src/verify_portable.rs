//! Portable identity for explicit, clean-checkout verification evidence.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::{cache, config, doctor, git, project};

#[path = "verify_portable_env.rs"]
mod environment;
#[path = "verify_portable_workspace.rs"]
mod workspace_inputs;

#[cfg(test)]
#[path = "verify_portable_policy_tests.rs"]
mod policy_tests;

pub(super) const SCHEMA_VERSION: u32 = 5;

/// The clean-checkout inputs a second clone compares. Values that could reveal machine
/// configuration are represented only by SHA-256 digests.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct PortableInputs {
    pub schema_version: u32,
    pub repository_sha256: String,
    pub head: String,
    pub toolchain: String,
    pub rustc_sha256: String,
    pub cargo_sha256: String,
    pub command_toolchains_sha256: String,
    pub environment_sha256: String,
    pub governor_sha256: String,
}

/// Capture an explicitly requested portable identity. Unsupported Cargo invocation forms
/// fail closed: ordinary local receipts still work, but another checkout must rerun them.
pub(super) fn capture(
    workspace: &Path,
    profile: &config::VerificationProfile,
    keep_debuginfo: bool,
    governor: config::Governor,
    governor_flags: Option<&str>,
) -> Result<Option<PortableInputs>> {
    if !profile.portable
        || !portable_profile(profile)
        || !portable_environment()
        || !doctor::portable_cargo_config_supported(workspace)?
        || !workspace_inputs::supported(workspace)?
    {
        return Ok(None);
    }
    let Some((head, remote)) = clean_head_and_remote(workspace) else {
        return Ok(None);
    };
    if ignored_files(workspace)? {
        return Ok(None);
    }
    validate_env(&profile.portable_env)?;
    let values = environment::child(&profile.portable_env, keep_debuginfo, governor_flags);
    let rustc = version(
        workspace,
        values
            .get(OsStr::new("RUSTC"))
            .cloned()
            .unwrap_or_else(|| "rustc".into()),
        &[],
        &values,
    )?;
    let cargo = version(workspace, "cargo".into(), &[], &values)?;
    Ok(Some(PortableInputs {
        schema_version: SCHEMA_VERSION,
        repository_sha256: digest(b"grove.portable.repository.v2\0", remote.as_bytes()),
        head,
        toolchain: project::toolchain(workspace),
        rustc_sha256: digest(b"grove.portable.rustc.v2\0", &rustc),
        cargo_sha256: digest(b"grove.portable.cargo.v2\0", &cargo),
        command_toolchains_sha256: command_toolchains(workspace, profile, &values)?,
        environment_sha256: environment(workspace, &values)?,
        governor_sha256: governor_digest(governor),
    }))
}

fn governor_digest(governor: config::Governor) -> String {
    let identity = format!(
        "{:?}:{}:{}",
        governor.mode, governor.cpu_slots, governor.max_builders
    );
    digest(b"grove.portable.governor.v1\0", identity.as_bytes())
}

/// Declared variables supplement the controlled standard child environment. Their values
/// are fingerprinted but never written to a receipt.
pub(super) fn validate_env(names: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            bail!("portable_env entries must use only uppercase letters, digits, and '_'")
        }
        if !seen.insert(name) {
            bail!("portable_env repeats {name:?}")
        }
    }
    Ok(())
}

fn portable_profile(profile: &config::VerificationProfile) -> bool {
    profile
        .commands
        .iter()
        .all(|command| supported(&command.argv))
}

/// Cross-checkout reuse has a deliberately hermetic Cargo subset. In particular,
/// Cargo plugins can resolve a mutable `cargo-*` executable from PATH, so they remain
/// local evidence until Grove can bind their exact executable inputs.
fn supported(argv: &[String]) -> bool {
    argv.first().is_some_and(|program| program == "cargo")
        && !argv.iter().any(|argument| unsupported(argument))
        && !custom_target(argv)
        && command_kind(argv).is_some()
}

fn custom_target(argv: &[String]) -> bool {
    argv.windows(2).any(
        |pair| matches!(pair, [flag, target] if flag == "--target" && target.ends_with(".json")),
    ) || argv.iter().any(|argument| {
        argument
            .strip_prefix("--target=")
            .is_some_and(|target| target.ends_with(".json"))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Metadata,
    Builds,
}

fn command_kind(argv: &[String]) -> Option<CommandKind> {
    let mut args = argv.iter().skip(1);
    let first = args.next()?;
    let first = if first.starts_with('+') {
        args.next()?
    } else {
        first
    };
    match first.as_str() {
        "-V" | "-Vv" | "--version" | "-h" | "--help" | "--list" => Some(CommandKind::Metadata),
        "build" | "check" | "test" | "bench" | "doc" => Some(CommandKind::Builds),
        "metadata" | "tree" => Some(CommandKind::Metadata),
        _ => None,
    }
}

fn unsupported(argument: &str) -> bool {
    argument == "--config"
        || argument.starts_with("--config=")
        || argument == "-C"
        || argument.starts_with("-C")
        || argument == "--manifest-path"
        || argument.starts_with("--manifest-path=")
        || argument == "--target-dir"
        || argument.starts_with("--target-dir=")
        || argument == "--lockfile-path"
        || argument.starts_with("--lockfile-path=")
        || argument == "-Z"
        || argument.starts_with("-Z")
}

fn portable_environment() -> bool {
    ![
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTDOC",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTDOC",
    ]
    .iter()
    .any(|name| env::var_os(name).is_some())
        && !env::vars_os().any(|(name, _)| {
            let name = name.to_string_lossy();
            name.starts_with("CARGO_ALIAS_")
                || (name.starts_with("CARGO_TARGET_")
                    && (name.ends_with("_LINKER") || name.ends_with("_RUNNER")))
        })
}

fn clean_head_and_remote(workspace: &Path) -> Option<(String, String)> {
    let status = git::capture(workspace, &["status", "--porcelain=v1", "-z"]).ok()?;
    if !status.is_empty() {
        return None;
    }
    let head = git::capture(workspace, &["rev-parse", "--verify", "HEAD"]).ok()?;
    let remote = git::capture(workspace, &["remote", "get-url", "origin"]).ok()?;
    normalize_remote(&remote).map(|remote| (head, remote))
}

fn ignored_files(workspace: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(workspace)
        .output()
        .context("listing ignored workspace inputs")?;
    if !output.status.success() {
        bail!(
            "listing ignored workspace inputs failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(!output.stdout.is_empty())
}

fn normalize_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.is_empty()
        || remote == "."
        || remote == ".."
        || remote.starts_with("./")
        || remote.starts_with("../")
    {
        return None;
    }
    let has_scheme = remote.contains("://");
    let scp_like = remote.contains(':');
    let absolute = Path::new(remote).is_absolute();
    (has_scheme || scp_like || absolute).then(|| remote.to_string())
}

fn command_toolchains(
    workspace: &Path,
    profile: &config::VerificationProfile,
    values: &BTreeMap<OsString, OsString>,
) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"grove.portable.command-toolchains.v1\0");
    for command in &profile.commands {
        let selected = command
            .argv
            .get(1)
            .filter(|argument| argument.starts_with('+'))
            .map(String::as_str);
        if selected.is_some() && values.contains_key(OsStr::new("RUSTC")) {
            bail!("portable verification does not combine RUSTC with cargo +toolchain")
        }
        let args = selected.into_iter().collect::<Vec<_>>();
        let rustc = version(workspace, "rustc".into(), &args, values)?;
        let cargo = version(workspace, "cargo".into(), &args, values)?;
        bytes(&mut hash, &rustc);
        bytes(&mut hash, &cargo);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn version(
    workspace: &Path,
    program: OsString,
    prefix: &[&str],
    values: &BTreeMap<OsString, OsString>,
) -> Result<Vec<u8>> {
    let output = Command::new(&program)
        .env_clear()
        .envs(values)
        .args(prefix)
        .arg("-Vv")
        .current_dir(workspace)
        .output()
        .with_context(|| format!("running {} -Vv", program.to_string_lossy()))?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!(
            "{} -Vv failed: {}",
            program.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(output.stdout)
}

fn environment(workspace: &Path, values: &BTreeMap<OsString, OsString>) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"grove.portable.environment.v4\0");
    part(&mut hash, "host-os", env::consts::OS.as_bytes());
    part(&mut hash, "host-arch", env::consts::ARCH.as_bytes());
    for (name, value) in values {
        bytes(&mut hash, name.as_encoded_bytes());
        bytes(&mut hash, value.as_encoded_bytes());
    }
    for (label, contents) in doctor::cargo_config_inputs(workspace)? {
        text(&mut hash, &label);
        bytes(&mut hash, &contents);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
fn effective_lane_environment(values: &mut BTreeMap<OsString, OsString>, keep_debuginfo: bool) {
    environment::effective_lane_environment(values, keep_debuginfo);
}

pub(super) fn configure_command(command: &mut Command, names: &[String], lane: &cache::Lane) {
    environment::configure_command(
        command,
        names,
        lane.keep_debuginfo,
        lane.governor_flags().as_deref(),
    );
    cache::apply_governor(command, lane);
}

pub(super) fn command_args(argv: &[String], lane: &cache::Lane) -> Vec<String> {
    environment::command_args(argv, lane)
}

fn digest(prefix: &[u8], value: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(prefix);
    bytes(&mut hash, value);
    format!("{:x}", hash.finalize())
}

fn part(hash: &mut Sha256, name: &str, value: &[u8]) {
    text(hash, name);
    bytes(hash, value);
}

fn text(hash: &mut Sha256, value: &str) {
    bytes(hash, value.as_bytes());
}

fn bytes(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn rejects_relative_remote_but_keeps_absolute_and_url_forms() {
        assert!(normalize_remote("../origin.git").is_none());
        assert!(normalize_remote("origin").is_none());
        assert!(normalize_remote("/srv/origin.git").is_some());
        assert!(normalize_remote("git@github.com:org/repo.git").is_some());
        assert!(normalize_remote("https://github.com/org/repo.git").is_some());
    }

    #[test]
    fn validates_explicit_environment_names() {
        assert!(validate_env(&["NEXUS_RELEASE_MODE".into()]).is_ok());
        assert!(validate_env(&["release_mode".into()]).is_err());
        assert!(validate_env(&["A".into(), "A".into()]).is_err());
    }

    #[test]
    fn rejects_cargo_configuration_that_cannot_be_bound() {
        assert!(unsupported("--config=config.toml"));
        assert!(unsupported("-Cother"));
        assert!(!unsupported("--release"));
    }

    #[test]
    fn rejects_external_cargo_subcommands() {
        assert!(supported(&args(&["cargo", "test"])));
        assert!(supported(&args(&["cargo", "+stable", "check"])));
        for argv in [&["cargo", "nextest", "run"][..], &["cargo", "fmt"]] {
            assert!(!supported(&args(argv)));
        }
    }

    #[test]
    fn rejects_opaque_cargo_inputs() {
        for argv in [
            &["cargo", "rustc"][..],
            &["cargo", "rustdoc"],
            &["cargo", "+nightly", "-Zbuild-std", "test"],
            &["cargo", "test", "-Z", "unstable-options"],
            &["cargo", "test", "--lockfile-path=/tmp/Cargo.lock"],
            &["cargo", "test", "--lockfile-path", "/tmp/Cargo.lock"],
            &["cargo", "test", "--target", "/tmp/custom.json"],
            &["cargo", "test", "--target=custom.json"],
        ] {
            assert!(!supported(&args(argv)));
        }
    }
}
