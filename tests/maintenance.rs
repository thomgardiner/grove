//! End-to-end cache maintenance around a Grove-owned command.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn init(repo: &Path) {
    fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='maintenance_fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "").unwrap();
}

#[test]
fn exec_collects_the_released_lane_after_its_command() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);

    let output = Command::new(GROVE)
        .args([
            "exec",
            "--tag",
            "maintenance",
            "--",
            "git",
            "rev-parse",
            "--is-inside-work-tree",
        ])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .env("GROVE_MIN_FREE_GB", "1000000")
        .output()
        .expect("run grove");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_dir(cache.join("lanes")).unwrap().next().is_none(),
        "post-command maintenance evicted the released lane"
    );
}
