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

/// Claude Code reads CLAUDE.md, not AGENTS.md, so the contract needs a bridge
/// there — one import line, never a second copy that would drift, and never
/// clobbering a repo's own CLAUDE.md content.
#[test]
fn init_bridges_claude_md_to_the_contract_without_clobbering() {
    // Absent: created containing only the import.
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fixture(&repo);
    assert!(run(&repo, &cache).status.success());
    let claude = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
    assert!(claude.contains("@AGENTS.md"), "{claude}");
    assert!(
        !claude.contains("grove:agents:v1"),
        "the bridge imports the contract; it must not copy it: {claude}"
    );

    // Existing content without a reference: appended below, preserved above.
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fixture(&repo);
    std::fs::write(repo.join("CLAUDE.md"), "# House rules\n\nBe kind.\n").unwrap();
    assert!(run(&repo, &cache).status.success());
    let claude = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
    assert!(claude.starts_with("# House rules"), "{claude}");
    assert!(claude.contains("Be kind."), "{claude}");
    assert!(claude.contains("@AGENTS.md"), "{claude}");

    // Re-running is idempotent: exactly one import line survives.
    assert!(run(&repo, &cache).status.success());
    let again = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap();
    assert_eq!(
        again.matches("@AGENTS.md").count(),
        1,
        "re-init must not stack bridges: {again}"
    );
}

/// no-clobber means no-clobber: an existing CLAUDE.md that cannot be read as
/// UTF-8 (or is otherwise unreadable) must not be silently overwritten with the
/// bridge. init fails loudly instead of destroying it.
#[test]
fn init_refuses_to_overwrite_an_unreadable_claude_md() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fixture(&repo);
    // Non-UTF-8 bytes: read_to_string fails with a kind that is NOT NotFound.
    std::fs::write(repo.join("CLAUDE.md"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
    let output = run(&repo, &cache);
    assert!(
        !output.status.success(),
        "init must fail rather than clobber an unreadable CLAUDE.md"
    );
    // The original bytes are untouched.
    assert_eq!(
        std::fs::read(repo.join("CLAUDE.md")).unwrap(),
        [0xff, 0xfe, 0x00, 0x01]
    );
}
