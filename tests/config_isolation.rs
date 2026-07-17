use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");
const OVERRIDES: &[&str] = &[
    "GROVE_CACHE_ROOT",
    "GROVE_MIN_FREE_GB",
    "GROVE_MAX_CANONICAL_GB",
    "GROVE_WORKTREE_ROOT",
    "GROVE_REAP_TTL_SECS",
    "GROVE_CLAIM_TTL_SECS",
    "GROVE_CPU_SLOTS",
    "GROVE_KEEP_DEBUGINFO",
    "GROVE_REQUIRE_COW",
];

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn init(repo: &Path, cache: &Path, worktrees: &Path, seed: u64) {
    fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "config@example.invalid"]);
    git(repo, &["config", "user.name", "config-test"]);
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        repo.join(".grove.toml"),
        format!(
            "cache_root = \"{}\"\n\
             min_free_gb = {seed}\n\
             max_canonical_gb = {}\n\
             worktree_root = \"{}\"\n\
             reap_ttl_secs = {}\n\
             claim_ttl_secs = {}\n\
             cpu_slots = {}\n\
             keep_debuginfo = {}\n\
             require_cow = {}\n",
            toml_path(cache),
            seed + 10,
            toml_path(worktrees),
            seed + 20,
            seed + 30,
            seed + 1,
            seed.is_multiple_of(2),
            !seed.is_multiple_of(2),
        ),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn command(repo: &Path, xdg: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(GROVE);
    command
        .args(args)
        .current_dir(repo)
        .env("XDG_CONFIG_HOME", xdg);
    for key in OVERRIDES {
        command.env_remove(key);
    }
    command.output().unwrap()
}

fn report(repo: &Path, xdg: &Path) -> Value {
    let output = command(repo, xdg, &["config"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_report(
    report: &Value,
    repo: &Path,
    global: &Path,
    cache: &Path,
    worktrees: &Path,
    seed: u64,
) {
    assert_eq!(report["workspace"], path(&fs::canonicalize(repo).unwrap()));
    assert_eq!(report["global_config"]["path"], path(global));
    assert_eq!(report["global_config"]["present"], true);
    assert_eq!(
        report["repository_config"]["path"],
        path(&fs::canonicalize(repo).unwrap().join(".grove.toml"))
    );
    assert_eq!(report["repository_config"]["present"], true);
    let effective = &report["effective"];
    assert_eq!(effective["cache_root"], path(cache));
    assert_eq!(effective["min_free_gb"], seed);
    assert_eq!(effective["min_free_bytes"], seed * 1024 * 1024 * 1024);
    assert_eq!(effective["max_canonical_gb"], seed + 10);
    assert_eq!(effective["worktree_root"], path(worktrees));
    assert_eq!(effective["reap_ttl_secs"], seed + 20);
    assert_eq!(effective["claim_ttl_secs"], seed + 30);
    assert_eq!(effective["cpu_slots"], seed + 1);
    assert_eq!(effective["keep_debuginfo"], seed.is_multiple_of(2));
    assert_eq!(effective["require_cow"], !seed.is_multiple_of(2));
}

#[test]
fn cli_configuration_and_cache_dispatch_stay_repository_local() {
    let base = tempdir().unwrap();
    let xdg = base.path().join("xdg");
    let global = xdg.join("grove/config.toml");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, "cpu_slots = 99\n").unwrap();
    let a = base.path().join("a");
    let b = base.path().join("b");
    let cache_a = base.path().join("cache-a");
    let cache_b = base.path().join("cache-b");
    let worktrees_a = base.path().join("worktrees-a");
    let worktrees_b = base.path().join("worktrees-b");
    init(&a, &cache_a, &worktrees_a, 2);
    init(&b, &cache_b, &worktrees_b, 3);
    let global_before = fs::read(&global).unwrap();
    let a_before = fs::read(a.join(".grove.toml")).unwrap();
    let b_before = fs::read(b.join(".grove.toml")).unwrap();

    assert_report(&report(&b, &xdg), &b, &global, &cache_b, &worktrees_b, 3);
    assert_report(&report(&a, &xdg), &a, &global, &cache_a, &worktrees_a, 2);
    for (repo, cache, agent) in [(&a, &cache_a, "a"), (&b, &cache_b, "b")] {
        let output = command(
            repo,
            &xdg,
            &["claim", "--agent", agent, "--task", "isolation", "src"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(cache.join("claims").exists());
    }
    assert_eq!(fs::read(&global).unwrap(), global_before);
    assert_eq!(fs::read(a.join(".grove.toml")).unwrap(), a_before);
    assert_eq!(fs::read(b.join(".grove.toml")).unwrap(), b_before);
}

#[test]
fn environment_overrides_every_reported_repository_policy() {
    let base = tempdir().unwrap();
    let xdg = base.path().join("xdg");
    let repo = base.path().join("repo");
    init(
        &repo,
        &base.path().join("configured-cache"),
        &base.path().join("configured-worktrees"),
        1,
    );
    let cache = base.path().join("override-cache");
    let worktrees = base.path().join("override-worktrees");
    let output = Command::new(GROVE)
        .arg("config")
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("GROVE_CACHE_ROOT", &cache)
        .env("GROVE_MIN_FREE_GB", "41")
        .env("GROVE_MAX_CANONICAL_GB", "42")
        .env("GROVE_WORKTREE_ROOT", &worktrees)
        .env("GROVE_REAP_TTL_SECS", "43")
        .env("GROVE_CLAIM_TTL_SECS", "44")
        .env("GROVE_CPU_SLOTS", "45")
        .env("GROVE_KEEP_DEBUGINFO", "true")
        .env("GROVE_REQUIRE_COW", "false")
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let effective = &report["effective"];
    assert_eq!(effective["cache_root"], path(&cache));
    assert_eq!(effective["min_free_gb"], 41);
    assert_eq!(effective["min_free_bytes"], 41 * 1024 * 1024 * 1024_u64);
    assert_eq!(effective["max_canonical_gb"], 42);
    assert_eq!(effective["worktree_root"], path(&worktrees));
    assert_eq!(effective["reap_ttl_secs"], 43);
    assert_eq!(effective["claim_ttl_secs"], 44);
    assert_eq!(effective["cpu_slots"], 45);
    assert_eq!(effective["keep_debuginfo"], true);
    assert_eq!(effective["require_cow"], false);
}
