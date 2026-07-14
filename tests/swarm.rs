//! Multi-process swarm tests: real `grove` processes hit shared state at the same time.
//! These exercise the cross-process flocks (the claim registry and the per-repo git
//! lock) that single-process tests cannot, since flock ownership is per open file.

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "swarm-test"]);
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    std::fs::write(
        dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Spawn `n` grove processes at once, then collect each `(exit_code, trimmed_stdout)`.
/// Spawning all before waiting is what makes them actually contend.
fn swarm(
    repo: &Path,
    cache: &Path,
    args: impl Fn(usize) -> Vec<String>,
    n: usize,
) -> Vec<(i32, String)> {
    let kids: Vec<_> = (0..n)
        .map(|i| {
            Command::new(GROVE)
                .args(args(i))
                .current_dir(repo)
                .env("GROVE_CACHE_ROOT", cache)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn grove")
        })
        .collect();
    kids.into_iter()
        .map(|k| {
            let out = k.wait_with_output().unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            )
        })
        .collect()
}

#[test]
fn thirty_two_agents_racing_one_scope_yield_exactly_one_winner() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    init_repo(&repo);
    let cache = base.path().join("cache");

    let results = swarm(
        &repo,
        &cache,
        |i| {
            vec![
                "claim".into(),
                "--agent".into(),
                format!("a{i}"),
                "crates/shared".into(),
            ]
        },
        32,
    );

    let granted = results.iter().filter(|(c, _)| *c == 0).count();
    let conflict = results.iter().filter(|(c, _)| *c == 1).count();
    assert_eq!(
        granted, 1,
        "exactly one agent wins the contested scope (got {granted})"
    );
    assert_eq!(
        conflict, 31,
        "everyone else sees a conflict (got {conflict})"
    );
}

#[test]
fn concurrent_acquires_get_distinct_worktrees_with_no_collision() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    init_repo(&repo);
    let cache = base.path().join("cache");
    let n = 16;

    let results = swarm(
        &repo,
        &cache,
        |_| {
            vec![
                "worktree".into(),
                "acquire".into(),
                "--agent".into(),
                "swarm".into(),
            ]
        },
        n,
    );

    for (code, out) in &results {
        assert_eq!(*code, 0, "an acquire failed: {out}");
    }
    let paths: Vec<&String> = results.iter().map(|(_, p)| p).collect();
    let unique: HashSet<&&String> = paths.iter().collect();
    assert_eq!(
        unique.len(),
        n,
        "every acquire got a distinct worktree, no two picked the same slot"
    );
    for p in &paths {
        assert!(Path::new(p).exists(), "worktree path {p} exists on disk");
    }
}
