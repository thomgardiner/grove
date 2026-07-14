//! End-to-end verification profiles, durable receipts, and task handoff evidence.

use serde_json::Value;
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
    git(repo, &["config", "user.email", "verify@example.test"]);
    git(repo, &["config", "user.name", "verify-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='verify_fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "#[cfg(test)] mod tests { #[test] fn present() {} }\n",
    )
    .unwrap();
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

fn begin(repo: &Path, cache: &Path) -> String {
    let output = run(
        repo,
        cache,
        &[
            "task", "begin", "--agent", "alice", "--task", "verify", "--scope", "src",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn required_profile_receipt_labels_only_the_matching_task_checkout_verified() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(
        &repo,
        r#"
[verification]
required = ["gate"]

[verification.profiles.gate]
continue_on_failure = false
commands = [{ argv = ["git", "rev-parse", "--verify", "HEAD"], allow_zero_tests = false }]
"#,
    );
    let missing = begin(&repo, &cache);
    let output = run(&repo, &cache, &["task", "finish", "--task-id", &missing]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let missing_report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(missing_report["task"]["verification"], "unverified");
    assert_eq!(
        missing_report["verification"]["missing"],
        serde_json::json!(["gate"])
    );

    let id = begin(&repo, &cache);
    let output = run(&repo, &cache, &["verify", "gate", "--task-id", &id]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verify: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(verify["passed"].as_bool().unwrap());
    assert_eq!(
        verify["receipts"][0]["argv"],
        serde_json::json!(["git", "rev-parse", "--verify", "HEAD"])
    );

    let output = run(&repo, &cache, &["task", "finish", "--task-id", &id]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let finished: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(finished["task"]["verification"], "passed");
    assert_eq!(
        finished["verification"]["passed"],
        serde_json::json!(["gate"])
    );
}

#[test]
fn failed_required_receipt_persists_across_a_second_finish() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(
        &repo,
        r#"
[verification]
required = ["bad"]

[verification.profiles.bad]
continue_on_failure = false
commands = [{ argv = ["git", "rev-parse", "--verify", "refs/heads/missing"], allow_zero_tests = false }]
"#,
    );
    let id = begin(&repo, &cache);
    let output = run(&repo, &cache, &["verify", "bad", "--task-id", &id]);
    assert_eq!(output.status.code(), Some(1));

    for _ in 0..2 {
        let output = run(&repo, &cache, &["task", "finish", "--task-id", &id]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let finished: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(finished["task"]["verification"], "failed");
        assert_eq!(
            finished["verification"]["failed"],
            serde_json::json!(["bad"])
        );
    }
}

#[test]
fn zero_selected_nextest_tests_cannot_make_a_successful_receipt() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(
        &repo,
        r#"
[verification.profiles.zero]
continue_on_failure = false
commands = [{ argv = ["cargo", "nextest", "run", "--workspace", "-E", "test(does_not_exist)", "--no-tests", "pass"], allow_zero_tests = false }]
"#,
    );
    let output = run(&repo, &cache, &["verify", "zero"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["receipts"][0]["test_count"], 0);
    assert_eq!(report["receipts"][0]["passed"], false);
}
