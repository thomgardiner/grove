//! Worktree reap must respect durable task ownership before it salvages or removes.

use fs2::FileExt;
use grove::worktree::{self, AcquireRequest};
use grove::{cache, project};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    git(path, &["init", "-q"]);
    git(
        path,
        &["config", "user.email", "worktree-task@example.invalid"],
    );
    git(path, &["config", "user.name", "worktree-task-test"]);
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(path.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        path.join("rust-toolchain.toml"),
        "[toolchain]\nchannel='stable'\n",
    )
    .unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-qm", "fixture"]);
    fs::canonicalize(path).unwrap()
}

fn acquire(root: &Path, repo: &Path, branch: &str) -> PathBuf {
    worktree::acquire(&AcquireRequest {
        root,
        cwd: repo,
        agent: "agent".into(),
        branch: Some(branch.into()),
        base: "HEAD".into(),
    })
    .unwrap()
}

fn run(workspace: &Path, root: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(workspace)
        .env("GROVE_CACHE_ROOT", root)
        .output()
        .unwrap()
}

fn spawn(workspace: &Path, root: &Path, args: &[&str]) -> Child {
    Command::new(GROVE)
        .args(args)
        .current_dir(workspace)
        .env("GROVE_CACHE_ROOT", root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn begin(workspace: &Path, root: &Path) -> String {
    let output = run(
        workspace,
        root,
        &[
            "task", "begin", "--agent", "agent", "--task", "fixture", "--scope", "src",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    report["task"]["id"].as_str().unwrap().to_string()
}

fn abandon(workspace: &Path, root: &Path, id: &str) {
    let output = run(
        workspace,
        root,
        &[
            "task",
            "abandon",
            "--task-id",
            id,
            "--reason",
            "fixture complete",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn lease_file(root: &Path) -> PathBuf {
    let mut entries = fs::read_dir(root.join("leases"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        });
    let path = entries.next().expect("one lease");
    assert!(entries.next().is_none(), "fixture created multiple leases");
    path
}

fn expire(root: &Path) {
    let path = lease_file(root);
    let mut lease: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    lease["created_at"] = 1.into();
    lease["last_activity"] = 1.into();
    cache::write_atomic(&path, &serde_json::to_vec_pretty(&lease).unwrap()).unwrap();
}

fn activity(root: &Path) -> u64 {
    let lease: Value = serde_json::from_slice(&fs::read(lease_file(root)).unwrap()).unwrap();
    lease["last_activity"].as_u64().unwrap()
}

fn status(workspace: &Path, root: &Path) -> Value {
    let output = run(workspace, root, &["task", "status", "--json"]);
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn wait_started(workspace: &Path, root: &Path, execution: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let report = status(workspace, root);
        if report["tasks"][0]["status"] == "active" {
            return;
        }
        if let Some(status) = execution.try_wait().unwrap() {
            let mut stderr = String::new();
            execution
                .stderr
                .as_mut()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!(
                "task exec exited before supervision ({status}): {stderr}; last report: {report}"
            );
        }
        if Instant::now() >= deadline {
            execution.kill().unwrap();
            panic!("task exec remained live but never published its command record");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn idle_task_blocks_reap_until_abandonment_then_dirty_work_is_salvaged() {
    let base = tempdir().unwrap();
    let repo = init(&base.path().join("repo"));
    let root = base.path().join("cache");
    let worktree = acquire(&root, &repo, "grove/idle-task");
    fs::write(worktree.join("wip.txt"), "preserve me").unwrap();
    let id = begin(&worktree, &root);
    expire(&root);
    let blocked = worktree::reap(&root, &repo, 0, false).unwrap();
    assert!(blocked.reaped.is_empty());
    assert_eq!(blocked.skipped.len(), 1);
    assert!(blocked.skipped[0].reason.contains(&id));
    assert_eq!(blocked.skipped[0].task_ids, [id.as_str()]);
    assert!(worktree.exists());
    assert_eq!(worktree::list(&root).len(), 1);

    abandon(&worktree, &root, &id);
    expire(&root);
    let reaped = worktree::reap(&root, &repo, 0, false).unwrap();
    assert_eq!(reaped.reaped.len(), 1);
    assert!(
        reaped.reaped[0]
            .saved_to
            .as_deref()
            .is_some_and(|reference| reference.starts_with("refs/grove/salvage/"))
    );
    assert!(!worktree.exists());
    let saved = Command::new("git")
        .args(["show", "grove/idle-task:wip.txt"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(saved.status.success());
    assert_eq!(saved.stdout, b"preserve me");
}

#[test]
fn heartbeat_makes_an_expired_lease_ineligible_for_reap() {
    let base = tempdir().unwrap();
    let repo = init(&base.path().join("repo"));
    let root = base.path().join("cache");
    let worktree = acquire(&root, &repo, "grove/heartbeat");
    expire(&root);
    worktree::heartbeat(&root, &worktree).unwrap();
    let report = worktree::reap(&root, &repo, 60, false).unwrap();
    assert!(report.reaped.is_empty());
    assert!(worktree.exists());
}

#[test]
fn reap_rechecks_a_lease_renewed_while_waiting_for_the_git_lock() {
    let base = tempdir().unwrap();
    let repo = init(&base.path().join("repo"));
    let root = base.path().join("cache");
    let worktree = acquire(&root, &repo, "grove/renewed");
    expire(&root);
    fs::remove_dir_all(root.join("lanes")).unwrap();
    let git_lock = File::create(root.join("locks").join(format!(
        "git-{}.lock",
        cache::repo_slug(&project::repo_identity(&repo))
    )))
    .unwrap();
    git_lock.lock_exclusive().unwrap();
    let lifecycle = fs::read_dir(root.join("locks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("lifecycle-")
        })
        .unwrap();
    let lifecycle = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lifecycle)
        .unwrap();
    let mut reap = spawn(&repo, &root, &["worktree", "reap", "--ttl", "60"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while FileExt::try_lock_exclusive(&lifecycle).is_ok() {
        FileExt::unlock(&lifecycle).unwrap();
        assert!(reap.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "reap never reached its Git lock");
        thread::sleep(Duration::from_millis(10));
    }
    let lease = lease_file(&root);
    let mut renewed: Value = serde_json::from_slice(&fs::read(&lease).unwrap()).unwrap();
    renewed["last_activity"] = u64::MAX.into();
    cache::write_atomic(&lease, &serde_json::to_vec_pretty(&renewed).unwrap()).unwrap();
    FileExt::unlock(&git_lock).unwrap();
    assert!(reap.wait().unwrap().success());
    assert!(worktree.exists() && lease.exists());
}

#[test]
fn malformed_or_unknown_task_state_blocks_reap_without_losing_evidence() {
    let base = tempdir().unwrap();
    let repo = init(&base.path().join("repo"));
    let root = base.path().join("cache");
    let worktree = acquire(&root, &repo, "grove/malformed-task");
    let id = begin(&worktree, &root);
    abandon(&worktree, &root, &id);
    expire(&root);
    let tasks = root
        .join("tasks")
        .join(cache::repo_slug(&project::repo_identity(&repo)));
    let record = tasks.join(format!("{id}.json"));
    let valid = fs::read(&record).unwrap();
    let evidence = b"{not valid task json";
    fs::write(&record, evidence).unwrap();
    let _ = status(&worktree, &root);
    let quarantined = record.with_extension("json.corrupt");
    let report = worktree::reap(&root, &repo, 0, false).unwrap();

    assert!(report.reaped.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("quarantined task record"));
    assert_eq!(fs::read(&quarantined).unwrap(), evidence);
    assert!(worktree.exists());

    fs::remove_file(quarantined).unwrap();
    let mut unknown: Value = serde_json::from_slice(&valid).unwrap();
    unknown["schema_version"] = 999.into();
    let evidence = serde_json::to_vec_pretty(&unknown).unwrap();
    cache::write_atomic(&record, &evidence).unwrap();
    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert!(report.reaped.is_empty());
    assert!(report.skipped[0].reason.contains("unsupported schema"));
    assert_eq!(fs::read(record).unwrap(), evidence);
    assert!(worktree.exists());
}

#[test]
fn duplicate_leases_refuse_ambiguous_cleanup_authority() {
    let base = tempdir().unwrap();
    let repo = init(&base.path().join("repo"));
    let root = base.path().join("cache");
    let worktree = acquire(&root, &repo, "grove/duplicate-lease");
    let lease = lease_file(&root);
    let duplicate = lease.with_file_name("duplicate.json");
    let foreign = lease.with_file_name("foreign.json");
    let mut alias: Value = serde_json::from_slice(&fs::read(&lease).unwrap()).unwrap();
    #[cfg(windows)]
    let alias_path = PathBuf::from(worktree.to_string_lossy().strip_prefix(r"\\?\").unwrap());
    #[cfg(not(windows))]
    let alias_path = worktree.join(".");
    assert_ne!(alias_path.as_os_str(), worktree.as_os_str());
    assert_eq!(cache::canonical_path(&alias_path), worktree);
    alias["workspace"] = alias_path.to_string_lossy().into_owned().into();
    cache::write_atomic(&duplicate, &serde_json::to_vec_pretty(&alias).unwrap()).unwrap();
    alias["repo"] = "another-repository".into();
    cache::write_atomic(&foreign, &serde_json::to_vec_pretty(&alias).unwrap()).unwrap();
    let report = worktree::reap(&root, &repo, 0, false).unwrap();

    assert!(report.reaped.is_empty());
    assert_eq!(report.skipped.len(), 2);
    let has = |text| report.skipped.iter().any(|skip| skip.reason.contains(text));
    assert!(has("ambiguous cleanup authority"));
    assert!(has("not canonical"));
    assert!(worktree.exists());
    assert!(lease.exists() && duplicate.exists() && foreign.exists());
}

#[test]
fn unmanaged_human_worktree_remains_outside_reap_authority() {
    let base = tempdir().unwrap();
    let repo = init(&base.path().join("repo"));
    let root = base.path().join("cache");
    let manual = base.path().join("manual");
    git(&repo, &["worktree", "add", "-q", manual.to_str().unwrap()]);

    let report = worktree::reap(&root, &repo, 0, false).unwrap();

    assert!(report.reaped.is_empty());
    assert!(manual.exists());
}

#[test]
#[ignore = "spawned by the live-supervisor integration test"]
fn supervised_child() {
    thread::sleep(Duration::from_secs(8));
}

#[test]
fn live_supervisor_pulses_the_lease_and_its_task_blocks_reap() {
    let base = tempdir().unwrap();
    let repo = init(&base.path().join("repo"));
    let root = base.path().join("cache");
    let worktree = acquire(&root, &repo, "grove/live-task");
    let id = begin(&worktree, &root);
    let test_binary = std::env::current_exe().unwrap();
    let mut execution = spawn(
        &worktree,
        &root,
        &[
            "task",
            "exec",
            "--task-id",
            &id,
            "--",
            test_binary.to_str().unwrap(),
            "--ignored",
            "--exact",
            "supervised_child",
        ],
    );
    wait_started(&worktree, &root, &mut execution);
    expire(&root);

    let deadline = Instant::now() + Duration::from_secs(7);
    while activity(&root) == 1 {
        assert!(
            Instant::now() < deadline,
            "five-second pulse never renewed lease"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let blocked = worktree::reap(&root, &repo, 0, false).unwrap();
    assert!(blocked.reaped.is_empty());
    assert_eq!(blocked.skipped.len(), 1);
    assert!(blocked.skipped[0].reason.contains(&id));
    assert_eq!(blocked.skipped[0].task_ids, [id.as_str()]);
    assert!(worktree.exists());

    let status = execution.wait().unwrap();
    assert!(status.success());
    abandon(&worktree, &root, &id);
}
