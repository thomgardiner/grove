#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init(repo: &Path, config: &str) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "dag@example.test"]);
    git(repo, &["config", "user.name", "dag-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='dag_fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn present() {}\n").unwrap();
    std::fs::write(repo.join(".grove.toml"), config).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn run(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .unwrap()
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[test]
fn dag_uses_independent_lanes_and_waits_for_dependencies() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    let marker = shell_path(&base.path().join("first-finished"));
    let first = toml(&format!("sleep 0.2; printf first > {marker}"));
    let after = toml(&format!("test -f {marker}"));
    init(
        &repo,
        &format!(
            r#"
[verification.profiles.dag]
continue_on_failure = true
max_parallel = 2
cpu_slots = 2
memory_mib = 64
commands = [
  {{ id = "first", argv = ["sh", "-c", "{first}"], allow_zero_tests = false, cpu = 1, memory_mib = 8 }},
  {{ id = "second", argv = ["sh", "-c", "sleep 0.2"], allow_zero_tests = false, cpu = 1, memory_mib = 8 }},
  {{ id = "after", needs = ["first"], argv = ["sh", "-c", "{after}"], allow_zero_tests = false, cpu = 1, memory_mib = 8 }},
]
"#,
        ),
    );
    let output = run(&repo, &cache, &["verify", "dag"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let receipts = report["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 3);
    assert!(receipts.iter().all(|receipt| receipt["passed"] == true));
    let lanes: BTreeSet<_> = receipts
        .iter()
        .map(|receipt| receipt["lane"]["tag"].as_str().unwrap())
        .collect();
    assert_eq!(lanes.len(), 3);
    assert!(lanes.iter().all(|lane| lane.starts_with("verify-dag-")));
}

#[test]
fn invalid_dag_is_rejected_before_any_command_runs() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    let marker = shell_path(&base.path().join("should-not-exist"));
    let command = toml(&format!("printf wrote > {marker}"));
    init(
        &repo,
        &format!(
            r#"
[verification.profiles.bad]
continue_on_failure = false
max_parallel = 2
cpu_slots = 2
commands = [
  {{ id = "a", needs = ["b"], argv = ["sh", "-c", "{command}"], allow_zero_tests = false }},
  {{ id = "b", needs = ["a"], argv = ["true"], allow_zero_tests = false }},
]
"#,
        ),
    );
    let output = run(&repo, &cache, &["verify", "bad"]);
    assert!(!output.status.success());
    assert!(!base.path().join("should-not-exist").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cycle"));
}
