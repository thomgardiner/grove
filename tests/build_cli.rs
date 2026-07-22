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

/// `why-rebuilt` must answer the question that actually costs time: after a
/// warm canonical exists, a clean tree rebuilds nothing, and a touched source
/// names itself. A parser that silently matched nothing would pass the first
/// half and fail the second.
#[test]
fn why_rebuilt_distinguishes_a_clean_tree_from_a_touched_source() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='whyrb'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
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

    let grove = |args: &[&str]| {
        Command::new(GROVE)
            .args(args)
            .current_dir(&repo)
            .env("GROVE_CACHE_ROOT", &cache)
            .output()
            .unwrap()
    };
    assert!(grove(&["cache", "warm"]).status.success());

    let clean = grove(&["why-rebuilt", "-p", "whyrb", "--json"]);
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let clean: serde_json::Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(
        clean["rebuilt"], 0,
        "a warm tree must rebuild nothing: {clean}"
    );
    assert!(clean["reused"].as_u64().unwrap() >= 1, "{clean}");

    fs::write(repo.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
    let dirty = grove(&["why-rebuilt", "-p", "whyrb", "--json"]);
    let dirty: serde_json::Value = serde_json::from_slice(&dirty.stdout).unwrap();
    assert!(
        dirty["rebuilt"].as_u64().unwrap() >= 1,
        "a touched source must rebuild something: {dirty}"
    );
    let unit = &dirty["explained"][0];
    assert_eq!(unit["package"], "whyrb");
    assert!(
        unit["explanation"]
            .as_str()
            .unwrap()
            .contains("input file changed"),
        "{unit}"
    );
    // Cargo reports the path in the platform's own form, so Windows says
    // `...\src\lib.rs`. Compare on normalized separators rather than assuming.
    assert!(
        unit["changed"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("src/lib.rs"),
        "{unit}"
    );
}

/// The regression this command exists for. A canonical that seeds nothing
/// reusable produces a cold rebuild that Cargo never reports as *dirty*, so
/// counting dirty units alone would call a dead cache healthy.
#[test]
fn why_rebuilt_fresh_reports_a_canonical_that_seeds_nothing() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='seedcheck'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
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

    let grove = |args: &[&str]| {
        Command::new(GROVE)
            .args(args)
            .current_dir(&repo)
            .env("GROVE_CACHE_ROOT", &cache)
            .output()
            .unwrap()
    };
    assert!(grove(&["cache", "warm"]).status.success());

    let healthy = grove(&["why-rebuilt", "-p", "seedcheck", "--fresh", "--json"]);
    let healthy: serde_json::Value = serde_json::from_slice(&healthy.stdout).unwrap();
    assert_eq!(
        healthy["rebuilt"], 0,
        "a freshly seeded lane must reuse the canonical: {healthy}"
    );

    // Strip the canonical's fingerprints: seeding still "succeeds" and the lane
    // still clones, but nothing in it can be reused.
    let mut stripped = 0;
    let mut stack = vec![cache.join("canonical")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|name| name == ".fingerprint") {
                fs::remove_dir_all(&path).unwrap();
                stripped += 1;
            } else {
                stack.push(path);
            }
        }
    }
    assert!(stripped > 0, "fixture must have had fingerprints to strip");

    let broken = grove(&["why-rebuilt", "-p", "seedcheck", "--fresh", "--json"]);
    let broken: serde_json::Value = serde_json::from_slice(&broken.stdout).unwrap();
    assert!(
        broken["rebuilt"].as_u64().unwrap() >= 1,
        "a canonical that seeds nothing must show a rebuild: {broken}"
    );
    assert_eq!(
        broken["reused"], 0,
        "nothing was reusable, so nothing may be counted reused: {broken}"
    );
    // The dirty log is silent here; only the artifact count reveals it.
    assert!(
        broken["explained"].as_array().unwrap().is_empty(),
        "cargo reports no stale unit for a cold build: {broken}"
    );
}
