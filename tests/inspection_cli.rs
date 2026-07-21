//! Portable end-to-end inspection lifecycle and capability contract.

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    );
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let base = tempdir().unwrap();
    let root = fs::canonicalize(base.path()).unwrap();
    let repo = root.join("repo");
    let cache = root.join("cache");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &["config", "user.email", "inspection@example.invalid"],
    );
    git(&repo, &["config", "user.name", "Inspection Test"]);
    fs::write(repo.join("candidate.txt"), b"candidate\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    (base, repo, cache)
}

fn run(repo: &Path, cache: &Path, args: &[String]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .unwrap()
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn begin(repo: &Path, cache: &Path) -> String {
    let output = run(
        repo,
        cache,
        &[
            "task",
            "begin",
            "--agent",
            "review-test",
            "--task",
            "review",
            "--scope",
            "candidate.txt",
        ]
        .map(String::from),
    );
    json(&output)["task"]["id"].as_str().unwrap().to_string()
}

fn acquire(repo: &Path, cache: &Path, task: &str, ttl: u64) -> Value {
    json(&run(
        repo,
        cache,
        &[
            "inspect".into(),
            "acquire".into(),
            "--task-id".into(),
            task.into(),
            "--ttl-secs".into(),
            ttl.to_string(),
        ],
    ))
}

fn child_args(test: &str) -> Vec<String> {
    vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "--ignored".into(),
        "--exact".into(),
        test.into(),
    ]
}

fn execute(repo: &Path, cache: &Path, capsule: &str, test: &str, timeout: u64) -> Output {
    let mut args = vec![
        "inspect".into(),
        "exec".into(),
        capsule.into(),
        "--timeout-secs".into(),
        timeout.to_string(),
        "--".into(),
    ];
    args.extend(child_args(test));
    run(repo, cache, &args)
}

#[test]
fn capabilities_distinguish_status_and_record_schemas() {
    let output = Command::new(GROVE).arg("capabilities").output().unwrap();
    let value = json(&output);
    assert_eq!(value["grove_version"], "0.3.3");
    assert_eq!(value["status"]["task_status_schema"], 2);
    assert_eq!(value["status"]["task_record_schema"], 4);
    assert_eq!(value["inspection"]["binding_schema"], 1);
}

#[test]
fn exact_capsule_exec_captures_logs_and_releases() {
    let (_base, repo, cache) = fixture();
    let task = begin(&repo, &cache);
    let acquired = acquire(&repo, &cache, &task, 60);
    let id = acquired["capsule_id"].as_str().unwrap();
    let report = json(&execute(&repo, &cache, id, "inspection_child_passes", 10));
    assert_eq!(report["authorized"], true);
    assert_eq!(report["source_unchanged"], true);
    assert_eq!(report["capsule_unchanged"], true);
    assert_eq!(report["stdout"]["truncated"], false);
    assert_eq!(report["stderr"]["truncated"], false);
    assert!(Path::new(report["stdout"]["path"].as_str().unwrap()).is_file());

    let released = json(&run(
        &repo,
        &cache,
        &["inspect".into(), "release".into(), id.into()],
    ));
    assert_eq!(released["released"], true);
}

#[test]
fn oversized_inspection_output_is_bounded_and_invalid() {
    let (_base, repo, cache) = fixture();
    let task = begin(&repo, &cache);
    let acquired = acquire(&repo, &cache, &task, 60);
    let output = execute(
        &repo,
        &cache,
        acquired["capsule_id"].as_str().unwrap(),
        "inspection_child_floods_output",
        10,
    );
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["authorized"], false);
    assert_eq!(report["stdout"]["truncated"], true);
    assert_eq!(report["stdout"]["bytes"], 1024 * 1024);
    let path = Path::new(report["stdout"]["path"].as_str().unwrap());
    assert_eq!(fs::metadata(path).unwrap().len(), 1024 * 1024);
    assert_eq!(report["stderr"]["truncated"], true);
    assert_eq!(report["stderr"]["bytes"], 1024 * 1024);
    let path = Path::new(report["stderr"]["path"].as_str().unwrap());
    assert_eq!(fs::metadata(path).unwrap().len(), 1024 * 1024);
}

#[test]
fn drift_timeout_and_capsule_mutation_fail_closed() {
    let (_base, repo, cache) = fixture();
    let task = begin(&repo, &cache);
    let acquired = acquire(&repo, &cache, &task, 60);
    fs::write(repo.join("candidate.txt"), b"changed after capture\n").unwrap();
    let drift = execute(
        &repo,
        &cache,
        acquired["capsule_id"].as_str().unwrap(),
        "inspection_child_passes",
        10,
    );
    assert_eq!(drift.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&drift.stderr).contains("changed before launch"));

    git(&repo, &["checkout", "--", "candidate.txt"]);
    let timeout_capsule = acquire(&repo, &cache, &task, 60);
    let timeout = execute(
        &repo,
        &cache,
        timeout_capsule["capsule_id"].as_str().unwrap(),
        "inspection_child_sleeps",
        0,
    );
    assert_eq!(timeout.status.code(), Some(1));
    let timeout: Value = serde_json::from_slice(&timeout.stdout).unwrap();
    assert_eq!(timeout["timed_out"], true);
    assert_eq!(timeout["exit_code"], 124);

    let mutation_capsule = acquire(&repo, &cache, &task, 60);
    let mutation = execute(
        &repo,
        &cache,
        mutation_capsule["capsule_id"].as_str().unwrap(),
        "inspection_child_mutates",
        10,
    );
    assert_eq!(mutation.status.code(), Some(1));
    let mutation: Value = serde_json::from_slice(&mutation.stdout).unwrap();
    assert_eq!(mutation["authorized"], false);
}

#[test]
fn expired_capsules_are_reaped_from_the_validated_namespace() {
    let (_base, repo, cache) = fixture();
    let task = begin(&repo, &cache);
    let acquired = acquire(&repo, &cache, &task, 1);
    std::thread::sleep(Duration::from_millis(1_100));
    let dry = json(&run(
        &repo,
        &cache,
        &["inspect".into(), "reap".into(), "--dry-run".into()],
    ));
    assert_eq!(dry["reaped"][0], acquired["capsule_id"]);
    let real = json(&run(&repo, &cache, &["inspect".into(), "reap".into()]));
    assert_eq!(real["reaped"][0], acquired["capsule_id"]);
}

#[test]
fn terminal_task_invalidates_an_unexecuted_capsule() {
    let (_base, repo, cache) = fixture();
    let task = begin(&repo, &cache);
    let acquired = acquire(&repo, &cache, &task, 60);
    let abandoned = run(
        &repo,
        &cache,
        &[
            "task".into(),
            "abandon".into(),
            "--task-id".into(),
            task,
            "--reason".into(),
            "candidate withdrawn".into(),
        ],
    );
    assert!(abandoned.status.success());
    let refused = execute(
        &repo,
        &cache,
        acquired["capsule_id"].as_str().unwrap(),
        "inspection_child_passes",
        10,
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("is not running"));
}

#[test]
#[ignore = "spawned by grove inspect exec"]
fn inspection_child_passes() {}

#[test]
#[ignore = "spawned by grove inspect exec"]
fn inspection_child_sleeps() {
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
#[ignore = "spawned by grove inspect exec"]
fn inspection_child_mutates() {
    fs::write("reviewer-mutation.txt", b"must invalidate review\n").unwrap();
}

#[test]
#[ignore = "spawned by grove inspect exec"]
fn inspection_child_floods_output() {
    std::io::stdout()
        .write_all(&vec![b'x'; 2 * 1024 * 1024])
        .unwrap();
    std::io::stderr()
        .write_all(&vec![b'y'; 2 * 1024 * 1024])
        .unwrap();
}
