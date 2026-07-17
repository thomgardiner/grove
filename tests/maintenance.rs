//! End-to-end cache maintenance around a Grove-owned command.

use fs2::FileExt;
use grove::cache;
use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::process::{Command, Output};
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

fn init_verified(repo: &Path) {
    init(repo);
    git(
        repo,
        &["config", "user.email", "maintenance@example.invalid"],
    );
    git(repo, &["config", "user.name", "maintenance-test"]);
    fs::write(
        repo.join(".grove.toml"),
        r#"
[verification]
required = []

[verification.profiles.gate]
continue_on_failure = false
commands = [
  { argv = ["git", "rev-parse", "--is-inside-work-tree"], allow_zero_tests = true },
]
"#,
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "fixture"]);
}

fn run(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .unwrap()
}

fn evidence_lock(cache: &Path) -> File {
    let path = cache.join("locks").join("verification-evidence.lock");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

fn records(cache: &Path, kind: &str) -> usize {
    let repo = fs::read_dir(cache.join(kind))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::read_dir(repo)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count()
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

#[test]
fn gc_spares_an_active_lane_and_shared_evidence_in_the_same_pass() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache_root = base.path().join("cache");
    init_verified(&repo);
    for _ in 0..2 {
        let output = run(&repo, &cache_root, &["verify", "gate"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(records(&cache_root, "verification-runs"), 2);

    let workspace = fs::canonicalize(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let lane = cache::acquire(&cache_root, &workspace, "stable").unwrap();
    let lane_id = lane.dir.file_name().unwrap().to_string_lossy().into_owned();
    let evidence = evidence_lock(&cache_root);
    FileExt::lock_shared(&evidence).unwrap();
    // SAFETY: nextest runs each test in its own process, so no sibling test observes it.
    unsafe { std::env::set_var("GROVE_MIN_FREE_GB", "1000000") };

    let blocked = cache::gc(&cache_root);
    assert!(lane.dir.exists(), "GC must not remove a locked lane");
    assert!(!blocked.evicted.contains(&lane_id));
    assert!(blocked.evidence_reclaimed.is_empty());
    assert_eq!(records(&cache_root, "verification-runs"), 2);

    drop(evidence);
    let resumed = cache::gc(&cache_root);
    unsafe { std::env::remove_var("GROVE_MIN_FREE_GB") };
    assert!(lane.dir.exists(), "the active lane remains protected");
    assert!(!resumed.evidence_reclaimed.is_empty());
    assert_eq!(records(&cache_root, "verification-runs"), 1);
}
