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
//! keep_debuginfo   = false                # keep debug info in lanes (default: off)
//! require_cow      = false                # refuse to seed if the clone would be a full copy
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub cache_root: Option<String>,
    pub min_free_gb: Option<u64>,
    pub max_canonical_gb: Option<u64>,
    pub worktree_root: Option<String>,
    pub reap_ttl_secs: Option<u64>,
    pub claim_ttl_secs: Option<u64>,
    pub keep_debuginfo: Option<bool>,
    pub require_cow: Option<bool>,
    pub verification: Option<VerificationConfig>,
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
    /// Must be declared so a profile's behavior after a failed command is never
    /// inferred from an implementation default.
    pub continue_on_failure: Option<bool>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationCommand {
    /// The literal program and arguments Grove executes in its verification lane.
    pub argv: Vec<String>,
    /// Must be declared. A selected test run with zero tests otherwise fails.
    pub allow_zero_tests: Option<bool>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// The resolved config (global merged with per-repo), loaded once.
pub fn get() -> &'static Config {
    CONFIG.get_or_init(load)
}

/// The global config file path, whether or not it exists.
pub fn global_path() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .map(|d| d.join("grove").join("config.toml"))
}

fn read(path: &Path) -> Option<Config> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn merge(base: &mut Config, over: Config) {
    base.cache_root = over.cache_root.or(base.cache_root.take());
    base.min_free_gb = over.min_free_gb.or(base.min_free_gb);
    base.max_canonical_gb = over.max_canonical_gb.or(base.max_canonical_gb);
    base.worktree_root = over.worktree_root.or(base.worktree_root.take());
    base.reap_ttl_secs = over.reap_ttl_secs.or(base.reap_ttl_secs);
    base.claim_ttl_secs = over.claim_ttl_secs.or(base.claim_ttl_secs);
    base.keep_debuginfo = over.keep_debuginfo.or(base.keep_debuginfo);
    base.require_cow = over.require_cow.or(base.require_cow);
    base.verification = over.verification.or(base.verification.take());
}

fn load() -> Config {
    let mut cfg = Config::default();
    if let Some(g) = global_path().and_then(|p| read(&p)) {
        merge(&mut cfg, g);
    }
    if let Some(r) = read(Path::new(".grove.toml")) {
        merge(&mut cfg, r);
    }
    cfg
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

/// Whether lanes keep debug info. Off by default (agents never need backtraces, and
/// dropping it is a large incremental-build win), so this is the opt-out for a human who
/// wants debuggable lane builds. `GROVE_KEEP_DEBUGINFO`, then config, then false.
pub fn keep_debuginfo() -> bool {
    env_bool("GROVE_KEEP_DEBUGINFO")
        .or(get().keep_debuginfo)
        .unwrap_or(false)
}

/// Whether seeding must be a true copy-on-write clone. Off by default (seed falls back to
/// a full copy where CoW is unavailable); on, seeding fails instead, so a machine on a
/// non-reflink filesystem builds cold rather than paying a full multi-gigabyte copy that
/// is slower than a cold build. `GROVE_REQUIRE_COW`, then config, then false.
pub fn require_cow() -> bool {
    env_bool("GROVE_REQUIRE_COW")
        .or(get().require_cow)
        .unwrap_or(false)
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
}
