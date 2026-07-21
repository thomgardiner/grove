use grove::api::Grove;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init(path: &Path, build: bool) {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        format!(
            "[package]\nname='retention_fixture'\nversion='0.1.0'\nedition='2024'\n{}",
            if build { "build='build.rs'\n" } else { "" }
        ),
    )
    .unwrap();
    fs::write(path.join("src/lib.rs"), "pub fn ready() {}\n").unwrap();
    if build {
        fs::write(
            path.join("build.rs"),
            "fn main() { let out = std::env::var_os(\"OUT_DIR\").unwrap(); std::fs::write(std::path::Path::new(&out).join(\"grove-sentinel\"), b\"warm\").unwrap(); }\n",
        )
        .unwrap();
    }
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "grove@example.test"]);
    git(path, &["config", "user.name", "Grove Test"]);
    git(path, &["add", "-A"]);
    git(path, &["commit", "-qm", "fixture"]);
    assert!(
        Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
}

fn contains(dir: &Path, name: &str) -> bool {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.file_name() == name || entry.path().is_dir() && contains(&entry.path(), name)
        })
}

fn run(repo: &Path, root: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", root)
        .output()
        .unwrap()
}

#[test]
fn promote_after_a_fresh_check_publishes_the_built_bootstrap() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let root = base.path().join("cache");
    init(&repo, true);

    let check = run(&repo, &root, &["check", "--package", "retention_fixture"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let promoted = run(&repo, &root, &["cache", "promote"]);
    assert!(
        promoted.status.success(),
        "{}",
        String::from_utf8_lossy(&promoted.stderr)
    );

    let grove = Grove::with_root(root, &repo);
    assert!(grove.published());
    assert!(
        contains(&grove.canonical(), "grove-sentinel"),
        "promotion must publish the lane the successful check warmed"
    );
}

#[test]
fn cargo_version_cannot_replace_a_warm_regular_lane() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let root = base.path().join("cache");
    init(&repo, false);
    let grove = Grove::with_root(root.clone(), &repo);
    let lane = grove.lane().unwrap();
    let regular = lane.dir.clone();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("warm.rlib"), b"warm").unwrap();
    drop(lane);

    let output = run(
        &repo,
        &root,
        &["exec", "--tag", "noop", "--", "cargo", "--version"],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        regular.exists(),
        "non-building Cargo is not retention evidence"
    );
}
