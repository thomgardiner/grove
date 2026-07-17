//! Stale-task recovery keeps its claim until Grove has a durable terminal record.

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

#[path = "recovery/authority.rs"]
mod recovery_authority;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "recovery@example.test"]);
    git(repo, &["config", "user.name", "recovery-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='recovery_fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
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

fn begin(repo: &Path, cache: &Path, scope: &str) -> Value {
    let output = run(
        repo,
        cache,
        &[
            "task", "begin", "--agent", "alice", "--task", "recover", "--scope", scope,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn dry_run_leaves_task_live_and_real_reap_records_terminal_state_before_releasing_claim() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let task = begin(&repo, &cache, "src");
    let id = task["task"]["id"].as_str().unwrap();

    let output = run(&repo, &cache, &["task", "reap", "--ttl", "0", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dry: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(dry["dry_run"].as_bool().unwrap());
    assert_eq!(dry["reaped"][0]["id"], id);

    let conflict = run(
        &repo,
        &cache,
        &[
            "task",
            "begin",
            "--agent",
            "bob",
            "--task",
            "blocked",
            "--scope",
            "src/lib.rs",
        ],
    );
    assert_eq!(
        conflict.status.code(),
        Some(1),
        "dry run must preserve the claim"
    );

    let output = run(&repo, &cache, &["task", "reap", "--ttl", "0"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reaped: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reaped["reaped"][0]["id"], id);
    assert_eq!(reaped["reaped"][0]["saved_to"], Value::Null);

    let status = run(&repo, &cache, &["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["tasks"][0]["status"], "abandoned");
    assert!(status["tasks"][0]["recovery"]["attempted_at"].is_number());
    assert!(
        run(
            &repo,
            &cache,
            &[
                "task",
                "begin",
                "--agent",
                "bob",
                "--task",
                "next",
                "--scope",
                "src/lib.rs",
            ],
        )
        .status
        .success()
    );
}

#[test]
fn leased_dirty_worktree_is_salvaged_before_its_task_claim_is_released() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let acquired = run(
        &repo,
        &cache,
        &["worktree", "acquire", "--agent", "recovery"],
    );
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );
    let worktree = String::from_utf8(acquired.stdout).unwrap();
    let worktree = Path::new(worktree.trim()).to_path_buf();
    std::fs::write(worktree.join("src/lib.rs"), "pub fn salvaged() {}\n").unwrap();
    let task = begin(&worktree, &cache, "src");
    let branch = task["task"]["branch"].as_str().unwrap().to_string();

    let output = run(&repo, &cache, &["task", "reap", "--ttl", "0"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reaped: Value = serde_json::from_slice(&output.stdout).unwrap();
    let reference = reaped["reaped"][0]["saved_to"].as_str().unwrap();
    assert!(reference.starts_with("refs/grove/salvage/"), "{reaped}");
    assert!(!worktree.exists());

    let archived = Command::new("git")
        .args(["show", &format!("{reference}:src/lib.rs")])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(archived.status.success());
    assert!(
        String::from_utf8_lossy(&archived.stdout).contains("salvaged"),
        "{}",
        String::from_utf8_lossy(&archived.stdout)
    );
    let saved = Command::new("git")
        .args(["show", &format!("{branch}:src/lib.rs")])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(saved.status.success());
    assert_eq!(saved.stdout, archived.stdout);
}
