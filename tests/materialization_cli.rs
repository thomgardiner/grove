#![allow(clippy::unwrap_used)]

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::{TempDir, tempdir};

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

struct Repo {
    _dir: TempDir,
    cache: PathBuf,
    source: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let source = dir.path().join("repo");
        write(
            &source,
            "Cargo.toml",
            "[workspace]\nmembers=['crates/*']\nresolver='2'\n",
        );
        for name in ["app", "tool", "large"] {
            write(
                &source,
                &format!("crates/{name}/Cargo.toml"),
                &format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n"),
            );
            write(
                &source,
                &format!("crates/{name}/src/lib.rs"),
                "pub fn marker() {}\n",
            );
        }
        write(
            &source,
            "crates/large/assets/payload.bin",
            &"payload".repeat(1024),
        );
        run(&source, "cargo", &["generate-lockfile"]);
        git(&source, &["init", "-q"]);
        git(&source, &["config", "core.autocrlf", "false"]);
        git(&source, &["config", "user.email", "cli@example.invalid"]);
        git(&source, &["config", "user.name", "CLI Test"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "base"]);
        Self {
            cache: dir.path().join("cache"),
            _dir: dir,
            source,
        }
    }

    fn grove(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(GROVE)
            .args(args)
            .current_dir(cwd)
            .env("GROVE_CACHE_ROOT", &self.cache)
            .output()
            .unwrap()
    }
}

#[test]
fn acquire_expand_and_full_are_one_monotonic_cli_flow() {
    let repo = Repo::new();
    let acquired = repo.grove(
        &repo.source,
        &[
            "worktree",
            "acquire",
            "--agent",
            "cli",
            "--materialize",
            "crate:app",
        ],
    );
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );
    let workspace = PathBuf::from(String::from_utf8(acquired.stdout).unwrap().trim());
    assert!(!workspace.join("crates/large/assets/payload.bin").exists());
    let sparse = status(&repo, &workspace);
    assert_eq!(sparse["mode"], "sparse");
    assert!(
        sparse["working_logical_bytes"].as_u64().unwrap()
            < sparse["full_working_logical_bytes"].as_u64().unwrap()
    );

    let expanded = repo.grove(
        &repo.source,
        &[
            "worktree",
            "expand",
            workspace.to_str().unwrap(),
            "crate:tool",
        ],
    );
    assert!(
        expanded.status.success(),
        "{}",
        String::from_utf8_lossy(&expanded.stderr)
    );
    assert!(workspace.join("crates/tool/src/lib.rs").is_file());
    assert!(!workspace.join("crates/large/assets/payload.bin").exists());

    let full = repo.grove(
        &repo.source,
        &["worktree", "full", workspace.to_str().unwrap()],
    );
    assert!(
        full.status.success(),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert!(workspace.join("crates/large/assets/payload.bin").is_file());
    let full = status(&repo, &workspace);
    assert_eq!(full["mode"], "full");
    assert_eq!(full["current_cones"], serde_json::json!([]));
    assert_eq!(
        full["working_logical_bytes"],
        full["full_working_logical_bytes"]
    );

    let released = repo.grove(
        &repo.source,
        &["worktree", "release", workspace.to_str().unwrap()],
    );
    assert!(
        released.status.success(),
        "{}",
        String::from_utf8_lossy(&released.stderr)
    );
}

fn status(repo: &Repo, workspace: &Path) -> Value {
    let output = repo.grove(&repo.source, &["status", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    report["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|worktree| worktree["path"] == workspace.to_string_lossy().as_ref())
        .and_then(|worktree| worktree.get("materialization"))
        .cloned()
        .expect("status reports materialization metrics")
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    run(dir, "git", args);
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
