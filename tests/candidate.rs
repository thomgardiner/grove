//! Candidate capture: immutable identity for task workspaces.

use serde_json::Value;
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

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "candidate-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='p'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn run(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .expect("run grove")
}

fn begin(repo: &Path, cache: &Path) -> String {
    let out = run(
        repo,
        cache,
        &[
            "task",
            "begin",
            "--agent",
            "cand",
            "--task",
            "capture",
            "--scope",
            "src/lib.rs",
        ],
    );
    assert!(
        out.status.success(),
        "begin: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["task"]["id"].as_str().unwrap().to_string()
}

fn head(repo: &Path) -> String {
    git_out(repo, &["rev-parse", "HEAD"])
}

fn capture(repo: &Path, cache: &Path, task_id: &str) -> Value {
    let out = run(
        repo,
        cache,
        &["candidate", "capture", "--task-id", task_id],
    );
    assert!(
        out.status.success(),
        "capture: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn clean_capture_binds_head_as_candidate_commit() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("r");
    let cache = tmp.path().join("c");
    init(&repo);
    let task_id = begin(&repo, &cache);
    let before = head(&repo);

    let cand = capture(&repo, &cache, &task_id);
    assert_eq!(cand["schema_version"], 1);
    assert_eq!(cand["task_id"], task_id);
    assert_eq!(cand["base_commit"], before);
    assert_eq!(cand["candidate_commit"], before);
    assert_eq!(cand["clean"], true);
    assert_eq!(cand["materialized"], false);
    assert_eq!(cand["index_represents_source"], true);
    assert_eq!(cand["source_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(cand["policy_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(cand["candidate_id"].as_str().unwrap().len(), 64);
    assert_eq!(head(&repo), before, "capture must not move HEAD");
    let retained = git_out(
        &repo,
        &[
            "rev-parse",
            &format!(
                "refs/grove/candidates/{}",
                cand["candidate_id"].as_str().unwrap()
            ),
        ],
    );
    assert_eq!(retained, before);

    let show = run(
        &repo,
        &cache,
        &["candidate", "show", cand["candidate_id"].as_str().unwrap()],
    );
    assert!(show.status.success());
    let loaded: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(loaded["candidate_id"], cand["candidate_id"]);
    assert_eq!(loaded["source_sha256"], cand["source_sha256"]);
}

#[test]
fn dirty_capture_materializes_and_retains_ref() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("r");
    let cache = tmp.path().join("c");
    init(&repo);
    let task_id = begin(&repo, &cache);
    let before = head(&repo);

    std::fs::write(repo.join("src/lib.rs"), "pub fn f() { /* dirty */ }\n").unwrap();
    git(&repo, &["add", "src/lib.rs"]);

    let cand = capture(&repo, &cache, &task_id);
    assert_eq!(cand["clean"], false);
    assert_eq!(cand["materialized"], true);
    assert_eq!(cand["base_commit"], before);
    assert_ne!(cand["candidate_commit"], before);
    assert_eq!(head(&repo), before, "materialization must not move HEAD");

    let parent = git_out(
        &repo,
        &["rev-parse", &format!("{}^", cand["candidate_commit"].as_str().unwrap())],
    );
    assert_eq!(parent, before);

    let retained = git_out(
        &repo,
        &[
            "rev-parse",
            &format!(
                "refs/grove/candidates/{}",
                cand["candidate_id"].as_str().unwrap()
            ),
        ],
    );
    assert_eq!(retained, cand["candidate_commit"].as_str().unwrap());

    // GC must not drop a retained candidate commit.
    git(&repo, &["gc", "--prune=now"]);
    let still = git_out(
        &repo,
        &["cat-file", "-t", cand["candidate_commit"].as_str().unwrap()],
    );
    assert_eq!(still, "commit");

    let show = run(
        &repo,
        &cache,
        &["candidate", "show", cand["candidate_id"].as_str().unwrap()],
    );
    assert!(
        show.status.success(),
        "show after gc: {}",
        String::from_utf8_lossy(&show.stderr)
    );
}

#[test]
fn untracked_content_changes_source_digest() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("r");
    let cache = tmp.path().join("c");
    init(&repo);
    let task_id = begin(&repo, &cache);

    let clean = capture(&repo, &cache, &task_id);
    std::fs::write(repo.join("src/extra.rs"), "pub fn g() {}\n").unwrap();
    let with_file = capture(&repo, &cache, &task_id);
    std::fs::write(repo.join("src/extra.rs"), "pub fn g() { /* changed */ }\n").unwrap();
    let with_edit = capture(&repo, &cache, &task_id);

    assert_ne!(clean["source_sha256"], with_file["source_sha256"]);
    assert_ne!(with_file["source_sha256"], with_edit["source_sha256"]);
    assert_eq!(with_file["index_represents_source"], false);
    assert!(with_file["untracked"].as_u64().unwrap() >= 1);
}

#[test]
fn unstaged_modification_marks_incomplete_and_materializes() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("r");
    let cache = tmp.path().join("c");
    init(&repo);
    let task_id = begin(&repo, &cache);
    let before = head(&repo);

    std::fs::write(repo.join("src/lib.rs"), "pub fn f() { /* unstaged */ }\n").unwrap();
    // no git add

    let cand = capture(&repo, &cache, &task_id);
    assert_eq!(cand["clean"], false);
    assert_eq!(cand["index_represents_source"], false);
    assert_eq!(cand["materialized"], true);
    assert_eq!(cand["base_commit"], before);
    assert_eq!(head(&repo), before);
}

#[test]
fn recapture_same_state_reuses_candidate_id() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("r");
    let cache = tmp.path().join("c");
    init(&repo);
    let task_id = begin(&repo, &cache);

    let a = capture(&repo, &cache, &task_id);
    let b = capture(&repo, &cache, &task_id);
    assert_eq!(a["candidate_id"], b["candidate_id"]);
    assert_eq!(a["source_sha256"], b["source_sha256"]);
    assert_eq!(a["candidate_commit"], b["candidate_commit"]);
}

#[test]
fn tampered_candidate_commit_refuses_show() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("r");
    let cache = tmp.path().join("c");
    init(&repo);
    let task_id = begin(&repo, &cache);
    let cand = capture(&repo, &cache, &task_id);
    let id = cand["candidate_id"].as_str().unwrap();

    // Find the persisted JSON and swap candidate_commit for an unrelated object.
    let slug = walkdir_find_json(&cache.join("candidates"), id);
    let mut v: Value = serde_json::from_slice(&std::fs::read(&slug).unwrap()).unwrap();
    // Point at tree object instead of commit — binding must fail.
    let tree = git_out(&repo, &["rev-parse", "HEAD^{tree}"]);
    v["candidate_commit"] = Value::String(tree);
    std::fs::write(&slug, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

    let show = run(&repo, &cache, &["candidate", "show", id]);
    assert!(!show.status.success());
}

#[test]
fn finished_task_refuses_capture() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("r");
    let cache = tmp.path().join("c");
    init(&repo);
    let task_id = begin(&repo, &cache);
    let fin = run(
        &repo,
        &cache,
        &[
            "task",
            "finish",
            "--task-id",
            &task_id,
            "--allow-unverified",
            "test",
        ],
    );
    assert!(
        fin.status.success(),
        "{}",
        String::from_utf8_lossy(&fin.stderr)
    );
    let out = run(
        &repo,
        &cache,
        &["candidate", "capture", "--task-id", &task_id],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not running") || err.contains("finished"),
        "stderr={err}"
    );
}

fn walkdir_find_json(dir: &Path, id: &str) -> std::path::PathBuf {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            if let Ok(found) = std::panic::catch_unwind(|| walkdir_find_json(&path, id)) {
                return found;
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(&format!("{id}.json")) {
            return path;
        }
    }
    panic!("candidate json not found for {id}");
}
