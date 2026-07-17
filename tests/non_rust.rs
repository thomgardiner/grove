//! The worktree-first contract: grove's coordination surface works in any git
//! repository, quietly, while the Rust acceleration suite declines with clear
//! messages instead of raw Cargo errors.

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
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// A JavaScript-shaped repository: git, package.json, no Cargo.toml anywhere.
fn init(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("package.json"), "{\"name\":\"webapp\"}\n").unwrap();
    std::fs::write(repo.join("src/index.js"), "console.log('hi');\n").unwrap();
    std::fs::write(
        repo.join(".grove.toml"),
        "[verification]\nrequired = [\"fast\"]\n[verification.profiles.fast]\ncontinue_on_failure = false\ncommands = [{ argv = [\"true\"], allow_zero_tests = true }]\n",
    )
    .unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "non-rust-test"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "init"]);
}

fn run(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .expect("run grove")
}

#[test]
fn full_task_lifecycle_is_quiet_without_cargo() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);

    let begun = run(
        &repo,
        &cache,
        &[
            "task", "begin", "--agent", "js", "--task", "t", "--scope", "src",
        ],
    );
    assert!(begun.status.success());
    let id = serde_json::from_slice::<Value>(&begun.stdout).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let exec = run(
        &repo,
        &cache,
        &["task", "exec", "--task-id", &id, "--", "true"],
    );
    assert!(exec.status.success());
    let verify = run(&repo, &cache, &["verify", "fast", "--task-id", &id]);
    assert!(verify.status.success());
    let finish = run(&repo, &cache, &["task", "finish", "--task-id", &id]);
    assert!(finish.status.success());

    // The whole lifecycle must not mention Cargo problems on stderr.
    for (name, output) in [
        ("begin", &begun),
        ("exec", &exec),
        ("verify", &verify),
        ("finish", &finish),
    ] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("incremental build policy") && !stderr.contains("Cargo.toml"),
            "{name} was noisy: {stderr}"
        );
    }
}

#[test]
fn rust_only_surfaces_decline_with_clear_messages() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);

    let doctor = run(&repo, &cache, &["doctor"]);
    assert!(doctor.status.success());
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert!(report["rust"].is_null());
    assert!(
        report["note"]
            .as_str()
            .is_some_and(|note| note.contains("not a Cargo workspace"))
    );

    let warm = run(&repo, &cache, &["cache", "warm"]);
    assert_eq!(warm.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&warm.stderr).contains("not a Cargo workspace"),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );

    let claimed = run(
        &repo,
        &cache,
        &["claim", "--agent", "js", "--task", "t", "crate:leftpad"],
    );
    assert!(!claimed.status.success());
    assert!(
        String::from_utf8_lossy(&claimed.stderr).contains("use repo-relative path scopes"),
        "{}",
        String::from_utf8_lossy(&claimed.stderr)
    );
}

#[test]
fn init_contract_is_language_appropriate() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);

    assert!(run(&repo, &cache, &["init"]).status.success());
    let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
    assert!(agents.contains("Coordinate before writing"));
    assert!(
        !agents.contains("plain cargo") && !agents.contains("grove check"),
        "cargo build rules leaked into a non-Rust contract"
    );
}
