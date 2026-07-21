#![allow(clippy::unwrap_used)]

use grove::config::Config;
use grove::worktree::{self, AcquireRequest};
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
            "[verification]\nrequired=[]\n\n[verification.profiles.gate]\ncontinue_on_failure=false\ncommands=[{ argv=['git','rev-parse','--verify','HEAD'], allow_zero_tests=false }]\n",
        );
        run(&source, "cargo", &["generate-lockfile"]);
        git(&source, &["init", "-q"]);
        git(&source, &["config", "core.autocrlf", "false"]);
        git(&source, &["config", "user.email", "opaque@example.invalid"]);
        git(&source, &["config", "user.name", "Opaque Test"]);
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
}

#[test]
fn direct_exec_converts_full_before_the_command_lane() {
    let repo = Repo::new();
    let workspace = repo.acquire("exec");

    let output = repo.grove(
        &workspace,
        &["exec", "--", "git", "rev-parse", "--verify", "HEAD"],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.join("crates/large/assets/payload.bin").is_file());
    worktree::release(&repo.cache, &workspace).unwrap();
}

#[test]
fn verification_converts_full_before_snapshot_and_lanes() {
    let repo = Repo::new();
    let workspace = repo.acquire("verify");

    let output = repo.grove(&workspace, &["verify", "gate"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.join("crates/large/assets/payload.bin").is_file());
    worktree::release(&repo.cache, &workspace).unwrap();
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
