use super::*;
use grove::cache;
use std::fs;
use std::path::PathBuf;

fn only_file(path: &Path) -> PathBuf {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path());
    let path = entries.next().expect("one durable record");
    assert!(entries.next().is_none(), "fixture created multiple records");
    path
}

fn lease_file(root: &Path) -> PathBuf {
    only_file(&root.join("leases"))
}

fn task_file(root: &Path) -> PathBuf {
    let repo = only_file(&root.join("tasks"));
    only_file(&repo)
}

fn acquisition_intent(root: &Path, lease: &Value) -> PathBuf {
    root.join("acquisitions")
        .join(cache::repo_slug(lease["repo"].as_str().unwrap()))
        .join(format!(
            "{}.json",
            cache::repo_slug(lease["workspace"].as_str().unwrap())
        ))
}

fn materialization_intent(root: &Path, lease: &Value) -> PathBuf {
    root.join("materializations")
        .join(cache::repo_slug(lease["repo"].as_str().unwrap()))
        .join(format!(
            "{}.json",
            cache::lane_id(
                lease["workspace"].as_str().unwrap(),
                lease["toolchain"].as_str().unwrap()
            )
        ))
}

fn write_blocker(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"preserve unresolved authority\n").unwrap();
}

fn acquire(repo: &Path, root: &Path, agent: &str) -> PathBuf {
    let output = run(repo, root, &["worktree", "acquire", "--agent", agent]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn assert_blocked(report: &Value, reason: &str) {
    assert!(report["reaped"].as_array().unwrap().is_empty());
    assert!(
        report["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains(reason)
    );
}

#[test]
fn dry_run_and_real_reap_agree_when_salvage_would_refuse() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let acquired = run(
        &repo,
        &cache,
        &["worktree", "acquire", "--agent", "blocked-recovery"],
    );
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );
    let worktree = String::from_utf8(acquired.stdout).unwrap();
    let worktree = Path::new(worktree.trim()).to_path_buf();
    let task = begin(&worktree, &cache, "src");
    let id = task["task"]["id"].as_str().unwrap();
    fs::write(worktree.join("intent.txt"), "future\n").unwrap();
    git(&worktree, &["add", "-N", "intent.txt"]);

    let dry = run(&repo, &cache, &["task", "reap", "--ttl", "0", "--dry-run"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert!(dry["reaped"].as_array().unwrap().is_empty());
    assert_eq!(dry["skipped"][0]["id"], id);
    assert!(
        dry["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("intent-to-add")
    );

    let actual = run(&repo, &cache, &["task", "reap", "--ttl", "0"]);
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    let actual: Value = serde_json::from_slice(&actual.stdout).unwrap();
    assert!(actual["reaped"].as_array().unwrap().is_empty());
    assert_eq!(actual["skipped"][0]["id"], id);
    assert!(
        actual["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("intent-to-add")
    );
    assert!(worktree.join("intent.txt").is_file());
}

#[test]
fn cleanup_authority_blockers_refuse_explicit_release() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let root = base.path().join("cache");
    init(&repo);
    let worktree = acquire(&repo, &root, "authority");
    let lease_path = lease_file(&root);
    let mut lease: Value = serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();

    for (intent, expected) in [
        (acquisition_intent(&root, &lease), "acquisition intent"),
        (
            materialization_intent(&root, &lease),
            "materialization intent",
        ),
    ] {
        write_blocker(&intent);
        let release = run(
            &repo,
            &root,
            &["worktree", "release", worktree.to_str().unwrap()],
        );
        assert!(!release.status.success());
        assert!(String::from_utf8_lossy(&release.stderr).contains(expected));
        assert!(worktree.exists() && intent.exists() && lease_path.exists());
        fs::remove_file(intent).unwrap();
    }

    lease["materialization"] = serde_json::json!({
        "schema_version": 1,
        "mode": "full",
        "requested_scopes": [],
        "closure_cones": [],
        "support_cones": [],
        "current_cones": ["src"],
        "base_oid": lease["base_oid"],
        "source_cargo_fingerprint": null,
        "candidate_cargo_fingerprint": null,
        "full_tracked_files": 0,
        "full_git_blob_bytes": 0,
        "full_working_files": 0,
        "full_working_logical_bytes": 0,
        "selected_tracked_files": 0,
        "selected_git_blob_bytes": 0,
        "working_files": 0,
        "working_logical_bytes": 0,
        "materialization_duration_ms": 0,
        "materialized_at": 1,
        "expansion_count": 0,
        "last_expanded_at": null,
        "fallback_reason": null
    });
    cache::write_atomic(&lease_path, &serde_json::to_vec_pretty(&lease).unwrap()).unwrap();
    let release = run(
        &repo,
        &root,
        &["worktree", "release", worktree.to_str().unwrap()],
    );
    assert!(!release.status.success());
    assert!(String::from_utf8_lossy(&release.stderr).contains("contradictory materialization"));
    assert!(worktree.exists() && lease_path.exists());
}

#[test]
fn task_reap_preflights_authority_before_changing_durable_state() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let root = base.path().join("cache");
    init(&repo);
    let worktree = acquire(&repo, &root, "task-authority");
    begin(&worktree, &root, "src");
    let task_path = task_file(&root);
    let before = fs::read(&task_path).unwrap();
    let lease: Value = serde_json::from_slice(&fs::read(lease_file(&root)).unwrap()).unwrap();
    let intent = acquisition_intent(&root, &lease);
    write_blocker(&intent);

    let reaped = run(&repo, &root, &["task", "reap", "--ttl", "0"]);
    assert!(reaped.status.success());
    let report: Value = serde_json::from_slice(&reaped.stdout).unwrap();
    assert_blocked(&report, "before durable state changed");
    assert_eq!(fs::read(task_path).unwrap(), before);
    assert!(worktree.exists() && intent.exists());
}

#[test]
fn future_matching_lease_is_not_treated_as_unmanaged_by_task_reap() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let root = base.path().join("cache");
    init(&repo);
    let worktree = acquire(&repo, &root, "future-lease");
    begin(&worktree, &root, "src");
    let task_path = task_file(&root);
    let before = fs::read(&task_path).unwrap();
    let lease_path = lease_file(&root);
    let mut lease: Value = serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
    lease["materialization"] = serde_json::json!({"schema_version": 999});
    let evidence = serde_json::to_vec_pretty(&lease).unwrap();
    cache::write_atomic(&lease_path, &evidence).unwrap();

    let reaped = run(&repo, &root, &["task", "reap", "--ttl", "0"]);
    assert!(reaped.status.success());
    let report: Value = serde_json::from_slice(&reaped.stdout).unwrap();
    assert_blocked(&report, "ambiguous cleanup authority");
    assert_eq!(fs::read(task_path).unwrap(), before);
    assert_eq!(fs::read(lease_path).unwrap(), evidence);
    assert!(worktree.exists());
}

#[test]
fn dry_run_preserves_malformed_task_record_in_place() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let root = base.path().join("cache");
    init(&repo);
    begin(&repo, &root, "src");
    let task = task_file(&root);
    let evidence = b"{malformed task record";
    fs::write(&task, evidence).unwrap();

    let reaped = run(&repo, &root, &["task", "reap", "--ttl", "0", "--dry-run"]);
    assert!(!reaped.status.success());
    assert!(String::from_utf8_lossy(&reaped.stderr).contains("read-only recovery"));
    assert_eq!(fs::read(&task).unwrap(), evidence);
    assert!(!task.with_extension("json.corrupt").exists());
}
