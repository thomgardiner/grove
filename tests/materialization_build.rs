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
        git(&source, &["config", "user.email", "build@example.invalid"]);
        git(&source, &["config", "user.name", "Build Test"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "base"]);
        Self {
            cache: dir.path().join("cache"),
            _dir: dir,
            source,
        }
    }

    fn acquire(&self) -> PathBuf {
        worktree::scoped(
            &AcquireRequest {
                root: &self.cache,
                cwd: &self.source,
                agent: "build".into(),
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
fn explicit_package_build_expands_before_lane_acquisition() {
    let repo = Repo::new();
    let workspace = repo.acquire();
    assert!(!workspace.join("crates/large/assets/payload.bin").exists());

    let output = repo.grove(&workspace, &["check", "-p", "tool"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.join("crates/tool/src/lib.rs").is_file());
    assert!(!workspace.join("crates/large/assets/payload.bin").exists());
    worktree::full(&repo.cache, &workspace).unwrap();
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
