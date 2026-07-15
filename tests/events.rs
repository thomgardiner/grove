//! The coordination lifecycle lands in the append-only event log orchestrators read.

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

fn fixture(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "e@example.test"]);
    git(repo, &["config", "user.name", "events-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='p'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "").unwrap();
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

#[test]
fn coordination_lifecycle_lands_in_the_event_log() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fixture(&repo);

    let claimed = run(&repo, &cache, &["claim", "--agent", "alice", "docs"]);
    assert!(claimed.status.success());
    let begun = run(
        &repo,
        &cache,
        &[
            "task", "begin", "--agent", "bob", "--task", "t", "--scope", "src",
        ],
    );
    assert!(
        begun.status.success(),
        "{}",
        String::from_utf8_lossy(&begun.stderr)
    );
    let id = serde_json::from_slice::<serde_json::Value>(&begun.stdout).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        run(
            &repo,
            &cache,
            &["task", "abandon", "--task-id", &id, "--reason", "test"]
        )
        .status
        .success()
    );
    assert!(
        run(&repo, &cache, &["release", "claims", "--agent", "alice"])
            .status
            .success()
    );

    let log = std::fs::read_dir(cache.join("events"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let names: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    for expected in [
        "claim.granted",
        "task.begun",
        "task.abandoned",
        "claim.released",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
}
