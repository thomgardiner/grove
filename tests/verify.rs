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
fn finish_requires_a_fresh_content_receipt_for_the_same_dirty_filename() {
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
    let id = begin(&repo, &cache);
    let output = run(&repo, &cache, &["task", "finish", "--task-id", &id]);
    assert!(!output.status.success());
    let conflict = run(
        &repo,
        &cache,
        &[
            "task",
            "begin",
            "--agent",
            "bob",
            "--task",
            "conflict",
            "--scope",
            "src/lib.rs",
        ],
    );
    assert_eq!(conflict.status.code(), Some(1));

    std::fs::write(repo.join("src/lib.rs"), "pub fn dirty() { 1; }\n").unwrap();
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
    let receipt_snapshot = verify["receipts"][0]["input"]["sha256"]
        .as_str()
        .unwrap()
        .to_string();

    std::fs::write(repo.join("src/lib.rs"), "pub fn dirty() { 2; }\n").unwrap();
    let stale = run(&repo, &cache, &["task", "finish", "--task-id", &id]);
    assert!(!stale.status.success());

    let output = run(&repo, &cache, &["verify", "gate", "--task-id", &id]);
    assert!(output.status.success());
    let refreshed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(
        refreshed["receipts"][0]["input"]["sha256"],
        receipt_snapshot
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
fn failed_required_receipt_keeps_the_task_running() {
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
        assert!(!output.status.success());
    }
    let conflict = run(
        &repo,
        &cache,
        &[
            "task",
            "begin",
            "--agent",
            "bob",
            "--task",
            "conflict",
            "--scope",
            "src/lib.rs",
        ],
    );
    assert_eq!(conflict.status.code(), Some(1));
}

#[test]
fn override_is_explicit_and_audited() {
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
    let id = begin(&repo, &cache);
    let output = run(
        &repo,
        &cache,
        &[
            "task",
            "finish",
            "--task-id",
            &id,
            "--allow-unverified",
            "release manager approved exception",
        ],
    );
    assert!(output.status.success());
    let finished: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(finished["task"]["verification"], "overridden");
    assert_eq!(
        finished["task"]["verification_reason"],
        "release manager approved exception"
    );
}

#[test]
fn out_of_scope_writes_block_finish_even_with_an_override() {
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
    let id = begin(&repo, &cache);
    std::fs::write(repo.join("README.md"), "outside task scope\n").unwrap();
    let output = run(
        &repo,
        &cache,
        &[
            "task",
            "finish",
            "--task-id",
            &id,
            "--allow-unverified",
            "not a scope override",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside its declared scope"));
}

#[cfg(unix)]
#[test]
fn verifier_that_writes_source_cannot_issue_a_passing_receipt() {
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
commands = [{ argv = ["sh", "-c", "printf 'changed\\n' > src/lib.rs"], allow_zero_tests = false }]
"#,
    );
    let id = begin(&repo, &cache);
    let output = run(&repo, &cache, &["verify", "gate", "--task-id", &id]);
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["receipts"][0]["passed"], false);
    assert_ne!(
        report["receipts"][0]["input"]["sha256"],
        report["receipts"][0]["output"]["sha256"]
    );
    assert!(
        !run(&repo, &cache, &["task", "finish", "--task-id", &id])
            .status
            .success()
    );
}

#[test]
fn untracked_drift_and_missing_snapshot_sidecar_fail_closed() {
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
    let id = begin(&repo, &cache);
    assert!(
        run(&repo, &cache, &["verify", "gate", "--task-id", &id])
            .status
            .success()
    );
    std::fs::write(repo.join("untracked-evidence.txt"), "present after verify").unwrap();
    assert!(
        !run(&repo, &cache, &["task", "finish", "--task-id", &id])
            .status
            .success()
    );
    std::fs::remove_file(repo.join("untracked-evidence.txt")).unwrap();

    let snapshots = cache.join("snapshots");
    let repo_snapshots = std::fs::read_dir(&snapshots)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let sidecar = std::fs::read_dir(repo_snapshots)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(sidecar, b"not a snapshot").unwrap();
    assert!(
        !run(&repo, &cache, &["task", "finish", "--task-id", &id])
            .status
            .success()
    );
}

#[test]
fn continued_profile_requires_every_command_in_the_same_run() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(
        &repo,
        r#"
[verification]
required = ["gate"]

[verification.profiles.gate]
continue_on_failure = true
commands = [
  { argv = ["git", "rev-parse", "--verify", "refs/heads/missing"], allow_zero_tests = false },
  { argv = ["git", "rev-parse", "--verify", "HEAD"], allow_zero_tests = false },
]
"#,
    );
    let id = begin(&repo, &cache);
    assert_eq!(
        run(&repo, &cache, &["verify", "gate", "--task-id", &id])
            .status
            .code(),
        Some(1)
    );
    assert!(
        !run(&repo, &cache, &["task", "finish", "--task-id", &id])
            .status
            .success()
    );
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
