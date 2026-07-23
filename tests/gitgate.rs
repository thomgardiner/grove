//! `grove git` serializes the writes that race concurrent worktrees on shared
//! `.git` state. Bare git in this exact scenario fails with `could not lock
//! config file: File exists` and `cannot lock ref` (reproduced by hand); the
//! gate must make every invocation succeed.

use std::path::Path;
use std::process::Command;
use std::thread;
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success(),
        "git {args:?} in {}",
        dir.display()
    );
}

/// Eight worktrees hammering the shared `.git` config and a shared tag through
/// `grove git` must never hit a lock error. The same load on bare git loses
/// writes; serialization is what makes a mixed-agent fleet safe.
#[test]
fn concurrent_shared_state_writes_through_grove_git_never_collide() {
    let base = tempdir().unwrap();
    let cache = base.path().join("cache");
    let main = base.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.invalid"]);
    git(&main, &["config", "user.name", "Gate Test"]);
    std::fs::write(main.join("f.txt"), "base\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "base"]);

    let worktrees: Vec<_> = (0..8)
        .map(|i| {
            let path = base.path().join(format!("wt{i}"));
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("wt{i}"),
                    path.to_str().unwrap(),
                    "HEAD",
                ],
            );
            path
        })
        .collect();

    let cache = cache.to_str().unwrap().to_string();
    let failures: Vec<String> = thread::scope(|scope| {
        let handles: Vec<_> = worktrees
            .iter()
            .enumerate()
            .map(|(i, wt)| {
                let cache = cache.clone();
                scope.spawn(move || {
                    let mut failures = Vec::new();
                    let run = |args: &[&str]| {
                        Command::new(GROVE)
                            .arg("git")
                            .arg("--")
                            .args(args)
                            .current_dir(wt)
                            .env("GROVE_CACHE_ROOT", &cache)
                            .output()
                            .unwrap()
                    };
                    for r in 0..20 {
                        // Both writers race the shared config file and a shared
                        // tag ref: exactly what bare git cannot survive.
                        let tag = run(&["tag", "-f", "shared-tag"]);
                        let cfg = run(&["config", &format!("grove.h{i}"), &r.to_string()]);
                        for (label, out) in [("tag", &tag), ("config", &cfg)] {
                            if !out.status.success() {
                                failures.push(format!(
                                    "wt{i} r{r} {label}: {}",
                                    String::from_utf8_lossy(&out.stderr).trim()
                                ));
                            }
                        }
                    }
                    failures
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });

    assert!(
        failures.is_empty(),
        "grove git must serialize shared-state writes; {} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A git hook that itself runs `grove git` must not deadlock: the outer
/// `grove git commit` holds the repository lock, and without the reentrancy
/// guard the hook's `grove git config` would block on that same lock forever.
#[test]
fn a_hook_running_grove_git_does_not_deadlock() {
    let base = tempdir().unwrap();
    let cache = base.path().join("cache");
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "Gate Test"]);
    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);

    // A pre-commit hook that performs a serialized write through grove. Point
    // core.hooksPath at a repo-local directory so this runs regardless of any
    // machine-global hooksPath (e.g. a shared pre-push gate).
    let hooks = base.path().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    git(
        &repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let hook = hooks.join("pre-commit");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nexec {GROVE:?} git -- config grove.hooktest ran\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    std::fs::write(repo.join("f.txt"), "changed\n").unwrap();
    // If reentrancy were broken this would hang; nextest's per-test timeout
    // would fail it rather than deadlocking the suite.
    let out = Command::new(GROVE)
        .args(["git", "--", "commit", "-am", "with hook"])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "commit under a grove-git hook must complete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let recorded = Command::new("git")
        .args(["config", "grove.hooktest"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&recorded.stdout).trim(),
        "ran",
        "the hook's serialized write must have run"
    );
}

/// Under `grove task exec`, a shim first on PATH makes the supervised command's
/// bare `git` route through the serialized gate automatically — the transparent
/// path that gives a whole fleet safe git without any agent knowing to call
/// `grove git`.
#[cfg(unix)]
#[test]
fn task_exec_puts_the_serializing_git_shim_first_on_path() {
    let base = tempdir().unwrap();
    let cache = base.path().join("cache");
    let repo = base.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='p'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "Gate Test"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    let grove = |args: &[&str]| {
        Command::new(GROVE)
            .args(args)
            .current_dir(&repo)
            .env("GROVE_CACHE_ROOT", &cache)
            .output()
            .unwrap()
    };
    let begun = grove(&[
        "task", "begin", "--agent", "a", "--task", "t", "--scope", "src",
    ]);
    assert!(
        begun.status.success(),
        "{}",
        String::from_utf8_lossy(&begun.stderr)
    );
    let begun: serde_json::Value = serde_json::from_slice(&begun.stdout).unwrap();
    let id = begun["task"]["id"].as_str().unwrap();

    // The supervised command resolves `git` to the shim, not the system git.
    let which = grove(&[
        "task",
        "exec",
        "--capability",
        "edit",
        "--task-id",
        id,
        "--",
        "sh",
        "-c",
        "command -v git",
    ]);
    assert!(
        which.status.success(),
        "{}",
        String::from_utf8_lossy(&which.stderr)
    );
    let resolved = String::from_utf8_lossy(&which.stdout);
    assert!(
        resolved.trim().ends_with("/gitshim/git"),
        "git under task exec must be the shim, got {resolved:?}"
    );

    // And a git write through that shim actually lands.
    let wrote = grove(&[
        "task",
        "exec",
        "--capability",
        "edit",
        "--task-id",
        id,
        "--",
        "sh",
        "-c",
        "git config grove.shimwrite ok && git config --get grove.shimwrite",
    ]);
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&wrote.stdout).trim(), "ok");
}
