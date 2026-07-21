use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn full_workspace_check_accepts_a_binary_only_package() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='binary_only'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    assert!(
        Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "grove@example.test"]);
    git(&repo, &["config", "user.name", "Grove Test"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "fixture"]);

    let output = Command::new(GROVE)
        .args(["check", "--files", "Cargo.toml"])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", base.path().join("cache"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
