//! Optional configuration. Every setting has a sensible default; a config file or an
//! environment variable can override it. Precedence, highest first:
//!
//!   env var  >  ./.grove.toml (per repo)  >  ~/.config/grove/config.toml (global)  >  default
//!
//! ```toml
//! # ~/.config/grove/config.toml, or a .grove.toml checked into a repo
//! cache_root       = "/fast-disk/grove"  # where lanes and canonicals live
//! min_free_gb      = 20                   # explicit reserve; default is 5% clamped to 20–50 GiB
//! max_canonical_gb = 40                   # cap total warm-build cache size
//! worktree_root    = "/work/worktrees"    # where `worktree acquire` puts worktrees
//! reap_ttl_secs    = 7200                 # idle time before a worktree is abandoned
//! claim_ttl_secs   = 1800                 # idle time before a work claim expires
//! cpu_slots        = 8                    # shared build token pool (default: core count)
//! keep_debuginfo   = false                # keep debug info in lanes (default: off)
//! require_cow      = false                # refuse to seed if the clone would be a full copy
//! ```

use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub cache_root: Option<String>,
    pub min_free_gb: Option<u64>,
    pub max_canonical_gb: Option<u64>,
    pub worktree_root: Option<String>,
    pub reap_ttl_secs: Option<u64>,
    pub claim_ttl_secs: Option<u64>,
    pub cpu_slots: Option<usize>,
    pub keep_debuginfo: Option<bool>,
    pub require_cow: Option<bool>,
    pub worktree: Option<WorktreeConfig>,
    pub verification: Option<VerificationConfig>,
}

impl Config {
    /// Resolve global configuration plus the nearest repository configuration for the
    /// explicit workspace. The result is owned so one process can bind many repositories.
    pub fn resolve(workspace: &Path) -> Self {
        resolve(Self::repository(workspace))
    }

    /// The nearest repository config for an explicit workspace, whether or not it parses.
    pub fn repository(workspace: &Path) -> Option<PathBuf> {
        workspace
            .ancestors()
            .map(|dir| dir.join(".grove.toml"))
            .find(|path| path.exists())
    }

    /// Effective cache root for this resolved configuration. Environment remains the
    /// highest precedence, followed by the bound config and the Cargo-home default.
    pub fn root(&self) -> PathBuf {
        std::env::var_os("GROVE_CACHE_ROOT")
            .map(PathBuf::from)
            .or_else(|| self.cache_root.as_ref().map(PathBuf::from))
            .unwrap_or_else(default_cache_root)
    }

    pub fn reserve(&self) -> Option<u64> {
        env_u64("GROVE_MIN_FREE_GB").or(self.min_free_gb)
    }

    pub fn budget(&self) -> Option<u64> {
        env_u64("GROVE_MAX_CANONICAL_GB").or(self.max_canonical_gb)
    }

    pub fn worktrees(&self) -> Option<PathBuf> {
        std::env::var_os("GROVE_WORKTREE_ROOT")
            .map(PathBuf::from)
            .or_else(|| self.worktree_root.as_deref().map(PathBuf::from))
    }

    pub fn reap(&self) -> u64 {
        env_u64("GROVE_REAP_TTL_SECS")
            .or(self.reap_ttl_secs)
            .unwrap_or(crate::worktree::DEFAULT_REAP_TTL_SECS)
    }

    pub fn claim(&self) -> u64 {
        env_u64("GROVE_CLAIM_TTL_SECS")
            .or(self.claim_ttl_secs)
            .unwrap_or(crate::claim::DEFAULT_CLAIM_TTL_SECS)
    }

    pub fn slots(&self) -> usize {
        std::env::var("GROVE_CPU_SLOTS")
            .ok()
            .and_then(|value| value.parse().ok())
            .or(self.cpu_slots)
            .filter(|slots| *slots > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|cores| cores.get())
                    .unwrap_or(4)
            })
    }

    pub fn debuginfo(&self) -> bool {
        env_bool("GROVE_KEEP_DEBUGINFO")
            .or(self.keep_debuginfo)
            .unwrap_or(false)
    }

    pub fn cow(&self) -> bool {
        env_bool("GROVE_REQUIRE_COW")
            .or(self.require_cow)
            .unwrap_or(false)
    }

    pub fn materialize(&self) -> Result<Vec<String>> {
        let Some(worktree) = &self.worktree else {
            return Ok(Vec::new());
        };
        let paths = worktree
            .materialize
            .iter()
            .map(|path| normalize_repo_path(path))
            .collect::<Result<BTreeSet<_>>>()?;
        Ok(paths.into_iter().collect())
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    pub materialize: Vec<String>,
}

pub(crate) fn normalize_repo_path(value: &str) -> Result<String> {
    let normalized = value.replace('\\', "/");
    let bytes = normalized.as_bytes();
    let windows_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if normalized.is_empty() || normalized.starts_with('/') || windows_prefix {
        bail!("worktree materialization path must be a nonempty repo-relative path")
    }
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("worktree materialization path must not escape the repository"),
            _ => parts.push(part),
        }
    }
    Ok(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    })
}

/// Repository-declared commands that establish a task's verification evidence. The
/// settings intentionally live in configuration rather than code: Grove records what
/// a repository chose to run, but does not decide whether a green command proves the
/// result is correct.
#[derive(Deserialize, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationConfig {
    /// Profiles a task must run against its current checkout before it can be labelled
    /// verified. An empty list preserves zero-setup operation.
    pub required: Vec<String>,
    pub profiles: BTreeMap<String, VerificationProfile>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationProfile {
    pub commands: Vec<VerificationCommand>,
    /// Permit reuse of this profile's clean receipt from a separate checkout. Profiles
    /// remain local by default because portability is an explicit repository contract.
    pub portable: bool,
    /// Named nonstandard inputs for review. Portable profiles fingerprint the complete
    /// command environment; values are never stored in receipts.
    pub portable_env: Vec<String>,
    /// Must be declared so a profile's behavior after a failed command is never
    /// inferred from an implementation default.
    pub continue_on_failure: Option<bool>,
    /// Maximum concurrent verification commands. Omit for the established serial lane.
    pub max_parallel: Option<usize>,
    /// Aggregate CPU slots available to this profile. Defaults to `max_parallel`.
    pub cpu_slots: Option<usize>,
    /// Optional aggregate memory budget in MiB.
    pub memory_mib: Option<u64>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationCommand {
    /// Stable DAG name. Omit for the deterministic `command-N` name.
    pub id: Option<String>,
    /// The literal program and arguments Grove executes in its verification lane.
    pub argv: Vec<String>,
    /// Must be declared. A selected test run with zero tests otherwise fails.
    pub allow_zero_tests: Option<bool>,
    /// Commands that must pass before this one may start.
    pub needs: Vec<String>,
    /// CPU slots consumed while this command is running (default 1).
    pub cpu: Option<usize>,
    /// Memory consumed while this command is running, in MiB (default 0).
    pub memory_mib: Option<u64>,
}

/// The global config file path, whether or not it exists.
pub fn global_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .map(|d| d.join("grove").join("config.toml"))
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    home_dir_for(
        cfg!(windows),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
    )
}

fn home_dir_for(
    windows: bool,
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Option<PathBuf> {
    if windows {
        user_profile.or(home)
    } else {
        home.or(user_profile)
    }
    .map(PathBuf::from)
}

/// Parse one config file. A missing file is silent (config is optional); a file that
/// exists but cannot be read or parsed is warned about loudly and skipped — safety
/// settings must never silently revert to their defaults.
fn read(path: &Path) -> Option<Config> {
    let text = match read_text(path) {
        Ok(Some(text)) => text,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("grove: cannot read config {}: {error}", path.display());
            return None;
        }
    };
    match toml::from_str(&text) {
        Ok(config) => Some(config),
        Err(error) => {
            eprintln!(
                "grove: ignoring config {}: {}",
                path.display(),
                error.message()
            );
            None
        }
    }
}

fn read_text(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn merge(base: &mut Config, over: Config) {
    base.cache_root = over.cache_root.or(base.cache_root.take());
    base.min_free_gb = over.min_free_gb.or(base.min_free_gb);
    base.max_canonical_gb = over.max_canonical_gb.or(base.max_canonical_gb);
    base.worktree_root = over.worktree_root.or(base.worktree_root.take());
    base.reap_ttl_secs = over.reap_ttl_secs.or(base.reap_ttl_secs);
    base.claim_ttl_secs = over.claim_ttl_secs.or(base.claim_ttl_secs);
    base.cpu_slots = over.cpu_slots.or(base.cpu_slots);
    base.keep_debuginfo = over.keep_debuginfo.or(base.keep_debuginfo);
    base.require_cow = over.require_cow.or(base.require_cow);
    base.worktree = over.worktree.or(base.worktree.take());
    base.verification = over.verification.or(base.verification.take());
}

fn resolve(repo: Option<PathBuf>) -> Config {
    let mut cfg = Config::default();
    if let Some(g) = global_path().and_then(|p| read(&p)) {
        merge(&mut cfg, g);
    }
    if let Some(r) = repo.and_then(|p| read(&p)) {
        merge(&mut cfg, r);
    }
    cfg
}

fn default_cache_root() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".cargo")
        })
        .join("grove")
}

/// Parse a boolean environment variable, accepting the common truthy/falsy spellings.
/// An unset or unrecognized value is `None`, so it falls through to the config or default.
fn env_bool(key: &str) -> Option<bool> {
    match std::env::var(key)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_repo_config_overrides_global_and_keeps_unset_globals() {
        let mut base = Config {
            cache_root: Some("/global/cache".into()),
            min_free_gb: Some(20),
            keep_debuginfo: Some(true),
            ..Config::default()
        };
        let over = Config {
            min_free_gb: Some(50),
            max_canonical_gb: Some(40),
            require_cow: Some(true),
            ..Config::default()
        };
        merge(&mut base, over);

        // Per-repo value wins where set.
        assert_eq!(base.min_free_gb, Some(50));
        assert_eq!(base.max_canonical_gb, Some(40));
        assert_eq!(base.require_cow, Some(true));
        // Global value is kept where the per-repo config leaves it unset.
        assert_eq!(base.cache_root.as_deref(), Some("/global/cache"));
        assert_eq!(base.keep_debuginfo, Some(true));
    }

    #[test]
    fn repo_config_is_found_from_a_subdirectory() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join(".grove.toml"), "min_free_gb = 7\n").unwrap();
        let deep = repo.path().join("src").join("nested");
        std::fs::create_dir_all(&deep).unwrap();

        let found = Config::repository(&deep).expect("ancestor walk finds the repo config");

        assert_eq!(
            crate::cache::canonical_path(&found),
            crate::cache::canonical_path(&repo.path().join(".grove.toml"))
        );
        assert_eq!(read(&found).unwrap().min_free_gb, Some(7));
    }

    #[test]
    fn unparseable_config_is_skipped_not_silently_defaulted_from() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".grove.toml");
        std::fs::write(&path, "min_free_gb = 7\nkeep_debug = true\n").unwrap();

        // The typo'd file is rejected whole (deny_unknown_fields) — read returns None
        // rather than a Config quietly missing the valid settings.
        assert!(read(&path).is_none());
    }

    #[test]
    fn home_resolution_uses_each_platforms_native_variable_first() {
        let home = Some(OsString::from("unix-home"));
        let profile = Some(OsString::from("windows-home"));
        assert_eq!(
            home_dir_for(false, home.clone(), profile.clone()),
            Some(PathBuf::from("unix-home"))
        );
        assert_eq!(
            home_dir_for(true, home, profile),
            Some(PathBuf::from("windows-home"))
        );
    }

    #[test]
    fn unreadable_config_is_not_treated_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_text(dir.path()).is_err());
    }
}
