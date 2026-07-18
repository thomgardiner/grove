use fs2::FileExt;
use grove::{cache, project, worktree};
use serde_json::Value;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(path.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "gate@example.invalid"]);
    git(path, &["config", "user.name", "Lifecycle Gate"]);
    git(path, &["add", "."]);
    git(path, &["commit", "-qm", "fixture"]);
    fs::canonicalize(path).unwrap()
}

fn holder(root: &Path, workspace: &Path, ready: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_grove"))
        .args([
            "exec",
            "--tag",
            "lifecycle-hold",
            "--",
            std::env::current_exe().unwrap().to_str().unwrap(),
            "--ignored",
            "--exact",
            "held_child",
        ])
        .current_dir(workspace)
        .env("GROVE_CACHE_ROOT", root)
        .env("GROVE_LIFECYCLE_READY", ready)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn run(root: &Path, workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grove"))
        .args(args)
        .current_dir(workspace)
        .env("GROVE_CACHE_ROOT", root)
        .output()
        .unwrap()
}

#[test]
fn release_observes_a_lane_held_by_another_process() {
    let base = tempdir().unwrap();
    let repo = repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let workspace = worktree::acquire(&worktree::AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "gate".into(),
        branch: Some("grove/gate".into()),
        base: "HEAD".into(),
    })
    .unwrap();
    let ready = base.path().join("holder-ready");
    let mut child = holder(&root, &workspace, &ready);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "lane holder never started");
        thread::sleep(Duration::from_millis(20));
    }

    let Err(error) = worktree::release(&root, &workspace) else {
        panic!("cross-process lane must block release")
    };
    assert!(error.to_string().contains("active build or tagged lane"));
    assert!(workspace.exists());
    assert!(child.wait().unwrap().success());
    worktree::release(&root, &workspace).unwrap();
    assert!(!workspace.exists());
}

#[test]
fn task_begin_and_cleanup_serialize_without_deadlock() {
    let base = tempdir().unwrap();
    let repo = repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let workspace = worktree::acquire(&worktree::AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "task-race".into(),
        branch: Some("grove/task-race".into()),
        base: "HEAD".into(),
    })
    .unwrap();
    let repo_id = project::repo_identity(&repo);
    fs::create_dir_all(root.join("locks")).unwrap();
    let registry = File::create(
        root.join("locks")
            .join(format!("claims-{}.lock", cache::repo_slug(&repo_id))),
    )
    .unwrap();
    registry.lock_exclusive().unwrap();
    let mut pending = Command::new(env!("CARGO_BIN_EXE_grove"))
        .args([
            "task", "begin", "--agent", "agent", "--task", "racing", "--scope", "src",
        ])
        .current_dir(&workspace)
        .env("GROVE_CACHE_ROOT", &root)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let snapshots = root.join("snapshots").join(cache::repo_slug(&repo_id));
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_dir(&snapshots)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true)
    {
        assert!(pending.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "task begin never reached publish"
        );
        thread::sleep(Duration::from_millis(10));
    }

    assert!(worktree::release(&root, &workspace).is_err());
    drop(registry);
    let output = pending.wait_with_output().unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = report["task"]["id"].as_str().unwrap();
    let error = worktree::release(&root, &workspace)
        .err()
        .expect("nonterminal task must block release");
    assert!(error.to_string().contains(id));
    let abandoned = run(
        &root,
        &workspace,
        &[
            "task",
            "abandon",
            "--task-id",
            id,
            "--reason",
            "test complete",
        ],
    );
    assert!(abandoned.status.success());
    worktree::release(&root, &workspace).unwrap();
}

#[test]
#[ignore = "spawned through grove exec by the lifecycle gate test"]
fn held_child() {
    fs::write(std::env::var_os("GROVE_LIFECYCLE_READY").unwrap(), b"ready").unwrap();
    thread::sleep(Duration::from_millis(750));
}
