#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use grove::config::Config;
use grove::worktree::{self, AcquireRequest};
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
        for name in ["app", "large"] {
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
        write(
            &source,
            ".grove.toml",
            "[verification]\nrequired=['release']\n\n[verification.profiles.release]\ncontinue_on_failure=false\ncommands=[{ argv=['sh','-c','test -f crates/large/assets/payload.bin && mkdir -p \"$CARGO_TARGET_DIR/release\" && printf ok > \"$CARGO_TARGET_DIR/release/tool\"'], allow_zero_tests=false }]\n",
        );
        run(&source, "cargo", &["generate-lockfile"]);
        git(&source, &["init", "-q"]);
        git(&source, &["config", "core.autocrlf", "false"]);
        git(
            &source,
            &["config", "user.email", "lifecycle@example.invalid"],
        );
        git(&source, &["config", "user.name", "Lifecycle Test"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "base"]);
        Self {
            cache: dir.path().join("cache"),
            _dir: dir,
            source,
        }
    }

    fn acquire(&self, agent: &str) -> PathBuf {
        worktree::scoped(
            &AcquireRequest {
                root: &self.cache,
                cwd: &self.source,
                agent: agent.into(),
                branch: None,
                base: "HEAD".into(),
            },
            &["crate:app".into()],
            &Config::resolve(&self.source),
        )
        .unwrap()
    }

    fn grove(&self, workspace: &Path, args: &[&str]) -> Output {
        Command::new(GROVE)
            .args(args)
            .current_dir(workspace)
            .env("GROVE_CACHE_ROOT", &self.cache)
            .output()
            .unwrap()
    }

    fn begin(&self, workspace: &Path) -> String {
        let output = self.grove(
            workspace,
            &[
                "task",
                "begin",
                "--agent",
                "lifecycle",
                "--task",
                "opaque",
                "--scope",
                "crates/app",
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
}

#[test]
fn task_exec_converts_full_before_recording_or_spawning() {
    let repo = Repo::new();
    let workspace = repo.acquire("task-exec");
    let id = repo.begin(&workspace);

    let output = repo.grove(
        &workspace,
        &[
            "task",
            "exec",
            "--task-id",
            &id,
            "--",
            "git",
            "rev-parse",
            "--verify",
            "HEAD",
        ],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.join("crates/large/assets/payload.bin").is_file());
    cleanup(&repo, &workspace, &id);
}

#[test]
fn release_freeze_converts_full_before_scope_snapshot_and_bundle() {
    let repo = Repo::new();
    let workspace = repo.acquire("release");
    let id = repo.begin(&workspace);
    let bundle = repo.source.parent().unwrap().join("bundle");

    let output = repo.grove(
        &workspace,
        &[
            "release",
            "freeze",
            "--task-id",
            &id,
            "--profile",
            "release",
            "--artifact",
            "target/release/tool",
            "--out",
            bundle.to_str().unwrap(),
        ],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.join("crates/large/assets/payload.bin").is_file());
    assert!(bundle.join("manifest.json").is_file());
    cleanup(&repo, &workspace, &id);
}

fn cleanup(repo: &Repo, workspace: &Path, id: &str) {
    let output = repo.grove(
        workspace,
        &["task", "abandon", "--task-id", id, "--reason", "test done"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    worktree::release(&repo.cache, workspace).unwrap();
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
