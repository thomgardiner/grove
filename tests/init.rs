//! `grove init` writes the agent contract without clobbering what a repo already has.

use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn fixture(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='p'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "").unwrap();
}

fn run(repo: &Path, cache: &Path) -> Output {
    Command::new(GROVE)
        .arg("init")
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .unwrap()
}

#[test]
fn init_writes_the_contract_once() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fixture(&repo);

    assert!(run(&repo, &cache).status.success());
    let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
    assert!(agents.contains("<!-- grove:agents:v1 -->"));
    assert!(agents.contains("grove worktree heartbeat PATH"));
    assert!(agents.contains("--materialize crate:<name>"));
    assert!(agents.contains("expansion never shrinks"));
    assert!(agents.contains("Sparse checkout is a size optimization, not a sandbox"));
    assert!(agents.contains("nonterminal tasks and live lanes also protect work"));
    assert!(agents.contains("JSONL is a low-latency best-effort signal"));
    assert!(agents.contains("rotation or write failure can create gaps"));
    assert!(agents.contains("reconcile durable task, claim, lease, and receipt state"));
    for file in [
        "RECURRING_BUGS.md",
        "DEBUG_RECIPES.md",
        "LESSONS_LEARNED.md",
    ] {
        assert!(agents.contains(file));
    }
    let config = std::fs::read_to_string(repo.join(".grove.toml")).unwrap();
    assert!(config.contains("[worktree]"));
    assert!(config.contains("materialize = [\"schemas/generated\"]"));

    let second = run(&repo, &cache);
    assert!(second.status.success());
    let report: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(report["written"].as_array().unwrap().len(), 0);
    assert_eq!(
        std::fs::read_to_string(repo.join("AGENTS.md"))
            .unwrap()
            .matches("grove:agents:v1")
            .count(),
        1
    );
}

#[test]
fn init_appends_below_an_existing_agents_md() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fixture(&repo);
    std::fs::write(repo.join("AGENTS.md"), "# Project rules\n\nkeep these\n").unwrap();

    assert!(run(&repo, &cache).status.success());
    let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("# Project rules"));
    assert!(agents.contains("keep these"));
    assert!(agents.contains("<!-- grove:agents:v1 -->"));
}
