//! Deterministic process-level proof of Grove's workspace → evidence → registry lock order.

use fs2::FileExt;
use grove::verify::Receipt;
use grove::{cache, project, snapshot};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
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

fn toml_string(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn init(repo: &Path, test_binary: &Path) -> PathBuf {
    fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    git(
        repo,
        &["config", "user.email", "concurrency@example.invalid"],
    );
    git(repo, &["config", "user.name", "concurrency-test"]);
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        repo.join("rust-toolchain.toml"),
        "[toolchain]\nchannel='stable'\n",
    )
    .unwrap();
    fs::write(
        repo.join(".grove.toml"),
        format!(
            "[verification]\nrequired=[]\n\n[verification.profiles.gate]\ncontinue_on_failure=false\ncommands=[{{ argv=[\"{}\", \"--ignored\", \"--exact\", \"verifier_barrier\"], allow_zero_tests=true }}]\n",
            toml_string(test_binary)
        ),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "fixture"]);
    fs::canonicalize(repo).unwrap()
}

fn command(repo: &Path, root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(GROVE);
    command
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn lock_file(path: &Path) -> File {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

fn wait_locked(lock: &File, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => FileExt::unlock(lock).unwrap(),
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => return,
            Err(error) => panic!("probing lock failed: {error}"),
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "task begin exited early"
        );
        assert!(
            Instant::now() < deadline,
            "task begin never acquired its workspace lock"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_snapshot(root: &Path, repo: &Path, child: &mut Child) {
    let dir = root
        .join("snapshots")
        .join(cache::repo_slug(&project::repo_identity(repo)));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_some()) {
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "task begin exited early"
        );
        assert!(
            Instant::now() < deadline,
            "task begin did not persist its snapshot before the registry lock"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn run(repo: &Path, root: &Path, args: &[&str]) -> Output {
    command(repo, root, args).output().unwrap()
}

fn wait(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "command timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn verify(repo: &Path, root: &Path) {
    let output = run(repo, root, &["verify", "gate"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_verify(repo: &Path, root: &Path, barrier: &Path, id: &str) -> Child {
    command(repo, root, &["verify", "gate"])
        .env("GROVE_CONCURRENCY_BARRIER", barrier)
        .env("GROVE_CONCURRENCY_ID", id)
        .spawn()
        .unwrap()
}

fn records(root: &Path, kind: &str, repo: &Path) -> Vec<PathBuf> {
    let dir = root
        .join(kind)
        .join(cache::repo_slug(&project::repo_identity(repo)));
    let mut records = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    records.sort();
    records
}

fn count(root: &Path, kind: &str, repo: &Path) -> usize {
    records(root, kind, repo).len()
}

fn wait_ready(barrier: &Path, left: &mut Child, right: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(barrier.join("a.ready").exists() && barrier.join("b.ready").exists()) {
        assert!(
            left.try_wait().unwrap().is_none(),
            "left verifier exited early"
        );
        assert!(
            right.try_wait().unwrap().is_none(),
            "right verifier exited early"
        );
        assert!(Instant::now() < deadline, "verifiers never rendezvoused");
        thread::sleep(Duration::from_millis(10));
    }
}

fn complete(root: &Path, repo: &Path) {
    let runs = records(root, "verification-runs", repo);
    let receipts = records(root, "receipts", repo);
    assert_eq!(runs.len(), 1);
    assert_eq!(receipts.len(), 1);
    let run: Value = serde_json::from_slice(&fs::read(&runs[0]).unwrap()).unwrap();
    let receipt: Receipt = serde_json::from_slice(&fs::read(&receipts[0]).unwrap()).unwrap();
    assert_eq!(run["run_id"], receipt.run_id);
    assert_eq!(run["command_count"], 1);
    assert_eq!(run["receipt_count"], 1);
    assert_eq!(run["passed"], true);
    assert!(receipt.passed);
    let evidence = receipt.evidence.expect("completed receipt has evidence");
    let repo_id = project::repo_identity(repo);
    snapshot::validate(root, &repo_id, &evidence.input).unwrap();
    snapshot::validate(root, &repo_id, &evidence.output).unwrap();
}

#[test]
#[ignore = "spawned by the lock-order integration test"]
fn verifier_barrier() {
    let Some(barrier) = std::env::var_os("GROVE_CONCURRENCY_BARRIER") else {
        return;
    };
    let id = std::env::var("GROVE_CONCURRENCY_ID").unwrap();
    let barrier = PathBuf::from(barrier);
    fs::create_dir_all(&barrier).unwrap();
    fs::write(barrier.join(format!("{id}.ready")), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(barrier.join("a.ready").exists()
        && barrier.join("b.ready").exists()
        && barrier.join("go").exists())
    {
        assert!(Instant::now() < deadline, "barrier was never released");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn task_begin_persists_its_snapshot_before_waiting_for_the_registry() {
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let repo = init(&base.path().join("repo"), &std::env::current_exe().unwrap());
    let evidence = lock_file(&root.join("locks/verification-evidence.lock"));
    evidence.lock_exclusive().unwrap();
    let registry = lock_file(&root.join("locks").join(format!(
        "claims-{}.lock",
        cache::repo_slug(&project::repo_identity(&repo))
    )));
    registry.lock_exclusive().unwrap();
    let workspace = lock_file(&root.join("locks").join(format!(
        "snapshot-workspace-{}.lock",
        cache::repo_slug(&repo.to_string_lossy())
    )));
    let mut begin = command(
        &repo,
        &root,
        &[
            "task",
            "begin",
            "--agent",
            "probe",
            "--task",
            "lock-order",
            "--scope",
            "src",
        ],
    )
    .spawn()
    .unwrap();
    wait_locked(&workspace, &mut begin);

    FileExt::unlock(&evidence).unwrap();
    wait_snapshot(&root, &repo, &mut begin);
    assert!(begin.try_wait().unwrap().is_none());

    FileExt::unlock(&registry).unwrap();
    let output = wait(begin, Duration::from_secs(10));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn publishers_share_evidence_while_gc_and_task_begin_make_progress() {
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let test_binary = std::env::current_exe().unwrap();
    let left_repo = init(&base.path().join("left"), &test_binary);
    let right_repo = init(&base.path().join("right"), &test_binary);
    let probe_repo = init(&base.path().join("probe"), &test_binary);
    let barrier = base.path().join("barrier");

    for repo in [&left_repo, &right_repo] {
        verify(repo, &root);
        verify(repo, &root);
        assert_eq!(count(&root, "verification-runs", repo), 2);
        assert_eq!(count(&root, "receipts", repo), 2);
    }

    let mut left = spawn_verify(&left_repo, &root, &barrier, "a");
    let mut right = spawn_verify(&right_repo, &root, &barrier, "b");
    wait_ready(&barrier, &mut left, &mut right);

    let gc = wait(
        command(&left_repo, &root, &["cache", "gc"])
            .spawn()
            .unwrap(),
        Duration::from_secs(5),
    );
    assert!(gc.status.success());
    let report: Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert!(report["evidence_reclaimed"].as_array().unwrap().is_empty());
    for repo in [&left_repo, &right_repo] {
        assert_eq!(count(&root, "verification-runs", repo), 2);
        assert_eq!(count(&root, "receipts", repo), 2);
    }

    let begin = wait(
        command(
            &probe_repo,
            &root,
            &[
                "task",
                "begin",
                "--agent",
                "probe",
                "--task",
                "lock-order",
                "--scope",
                "src",
            ],
        )
        .spawn()
        .unwrap(),
        Duration::from_secs(5),
    );
    assert!(
        begin.status.success(),
        "{}",
        String::from_utf8_lossy(&begin.stderr)
    );
    assert!(left.try_wait().unwrap().is_none());
    assert!(right.try_wait().unwrap().is_none());

    fs::write(barrier.join("go"), b"go").unwrap();
    for output in [
        wait(left, Duration::from_secs(10)),
        wait(right, Duration::from_secs(10)),
    ] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let gc = run(&left_repo, &root, &["cache", "gc"]);
    assert!(gc.status.success());
    let report: Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert!(!report["evidence_reclaimed"].as_array().unwrap().is_empty());
    complete(&root, &left_repo);
    complete(&root, &right_repo);
}
