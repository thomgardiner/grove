//! End-to-end task lifecycle, status, exit propagation, and crash supervision.

use fs2::FileExt;
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
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
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn init(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "task-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='p'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "").unwrap();
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

fn evidence_lock(cache: &Path) -> File {
    let locks = cache.join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(locks.join("verification-evidence.lock"))
        .unwrap()
}

fn wait_output(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "grove command timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn spawn(repo: &Path, cache: &Path, args: &[&str]) -> Child {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grove")
}

fn begin(repo: &Path, cache: &Path, scope: &str) -> String {
    let output = run(
        repo,
        cache,
        &[
            "task", "begin", "--agent", "alice", "--task", "feature", "--scope", scope,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    json["task"]["id"].as_str().unwrap().to_string()
}

fn status(repo: &Path, cache: &Path) -> Value {
    let output = run(repo, cache, &["status", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn wait_for(repo: &Path, cache: &Path, expected: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let report = status(repo, cache);
        if report["tasks"][0]["status"] == expected {
            return report;
        }
        assert!(
            Instant::now() < deadline,
            "task did not become {expected}: {report}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_active_command(repo: &Path, cache: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let report = status(repo, cache);
        let command = &report["tasks"][0]["commands"][0];
        if report["tasks"][0]["status"] == "active" && command["pid"].is_number() {
            return report;
        }
        assert!(
            Instant::now() < deadline,
            "task command did not publish an active pid: {report}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn shared_evidence_publication_does_not_block_task_begin() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let lock = evidence_lock(&cache);
    FileExt::lock_shared(&lock).unwrap();

    let output = wait_output(spawn(
        &repo,
        &cache,
        &[
            "task", "begin", "--agent", "alice", "--task", "feature", "--scope", "src",
        ],
    ));

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn task_begin_waits_for_evidence_before_taking_the_registry_lock() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let lock = evidence_lock(&cache);
    lock.lock_exclusive().unwrap();
    let begin = spawn(
        &repo,
        &cache,
        &[
            "task", "begin", "--agent", "alice", "--task", "feature", "--scope", "src",
        ],
    );
    thread::sleep(Duration::from_millis(100));

    let status = wait_output(spawn(&repo, &cache, &["status", "--json"]));
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );

    drop(lock);
    let output = wait_output(begin);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn shared_claim_group_overlaps_freely_but_blocks_outsiders() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);

    let begin_grouped = |agent: &str, group: &[&str]| {
        let mut args = vec![
            "task", "begin", "--agent", agent, "--task", "variant", "--scope", "src",
        ];
        args.extend_from_slice(group);
        run(&repo, &cache, &args)
    };

    // Two variant attempts at the same scope coexist inside one group.
    let first = begin_grouped("smn-a-codex", &["--claim-group", "order-a"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = begin_grouped("smn-a-glm", &["--claim-group", "order-a"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );

    // An outsider (no group, or another group) still conflicts with them.
    let outsider = begin_grouped("bob", &[]);
    assert_eq!(outsider.status.code(), Some(1));
    let other_group = begin_grouped("smn-b-codex", &["--claim-group", "order-b"]);
    assert_eq!(other_group.status.code(), Some(1));
}

#[test]
fn finish_is_idempotent_and_releases_the_task_claim() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    let conflict = run(
        &repo,
        &cache,
        &[
            "task",
            "begin",
            "--agent",
            "bob",
            "--task",
            "other",
            "--scope",
            "src/lib.rs",
        ],
    );
    assert_eq!(conflict.status.code(), Some(1));

    for _ in 0..2 {
        let output = run(
            &repo,
            &cache,
            &[
                "task",
                "finish",
                "--task-id",
                &id,
                "--allow-unverified",
                "fixture has no verification profile",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(status(&repo, &cache)["tasks"][0]["status"], "finished");
    let compact = run(&repo, &cache, &["task", "status", "--json"]);
    let compact: Value = serde_json::from_slice(&compact.stdout).unwrap();
    assert_eq!(compact["schema_version"], 3);
    assert_eq!(compact["tasks"][0]["recorded_verification"], "overridden");
    assert!(compact["tasks"][0]["source_sha256"].is_null());
    let active = run(&repo, &cache, &["task", "status", "--active", "--json"]);
    assert!(active.status.success());
    assert!(
        serde_json::from_slice::<Value>(&active.stdout).unwrap()["tasks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        run(
            &repo,
            &cache,
            &[
                "task",
                "begin",
                "--agent",
                "bob",
                "--task",
                "next",
                "--scope",
                "src/lib.rs",
            ],
        )
        .status
        .success()
    );
}

#[test]
fn exec_propagates_failure_and_records_exact_argv() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");
    let argv = [
        "task",
        "exec",
        "--task-id",
        &id,
        "--",
        "git",
        "rev-parse",
        "--verify",
        "refs/heads/definitely-missing",
    ];
    let output = run(&repo, &cache, &argv);
    assert_eq!(output.status.code(), Some(128));
    let report = status(&repo, &cache);
    assert_eq!(report["tasks"][0]["status"], "failed");
    assert_eq!(
        report["tasks"][0]["commands"][0]["argv"],
        serde_json::json!([
            "git",
            "rev-parse",
            "--verify",
            "refs/heads/definitely-missing"
        ])
    );
}

#[test]
fn staged_only_out_of_scope_write_blocks_finish() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    std::fs::write(repo.join("README.md"), "baseline\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "add readme"]);
    let id = begin(&repo, &cache, "src");

    std::fs::write(repo.join("README.md"), "staged\n").unwrap();
    git(&repo, &["add", "README.md"]);
    std::fs::write(repo.join("README.md"), "baseline\n").unwrap();
    let output = run(
        &repo,
        &cache,
        &[
            "task",
            "finish",
            "--task-id",
            &id,
            "--allow-unverified",
            "fixture has no verification profile",
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let refusal: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(refusal["outcome"], "refused");
    assert_eq!(refusal["reason"], "scope");
    assert_eq!(refusal["outside_scope"], serde_json::json!(["README.md"]));
}

#[test]
fn orphaned_live_child_keeps_task_active_and_blocks_finish() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");
    let mut grove = Command::new(GROVE)
        .args([
            "task",
            "exec",
            "--task-id",
            &id,
            "--",
            "git",
            "hash-object",
            "--stdin",
        ])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = grove.stdin.take().unwrap();
    input.write_all(b"still running").unwrap();
    let active = wait_for_active_command(&repo, &cache);
    assert!(active["tasks"][0]["commands"][0]["pid"].is_number());
    let concise = run(&repo, &cache, &["task", "status", "--active", "--json"]);
    assert!(concise.status.success());
    let concise: Value = serde_json::from_slice(&concise.stdout).unwrap();
    assert_eq!(concise["tasks"][0]["id"], id);
    assert_eq!(concise["tasks"][0]["owner"], "alice");
    assert_eq!(concise["tasks"][0]["scope"], serde_json::json!(["src"]));
    assert!(concise["tasks"][0]["heartbeat_age_secs"].is_number());
    assert_eq!(
        concise["tasks"][0]["active_command"]["argv"],
        serde_json::json!(["git", "hash-object", "--stdin"])
    );

    grove.kill().unwrap();
    grove.wait().unwrap();
    assert_eq!(
        wait_for(&repo, &cache, "active")["tasks"][0]["status"],
        "active"
    );
    assert!(
        !run(&repo, &cache, &["task", "finish", "--task-id", &id])
            .status
            .success()
    );

    drop(input);
    wait_for(&repo, &cache, "failed");
    assert!(
        run(
            &repo,
            &cache,
            &[
                "task",
                "finish",
                "--task-id",
                &id,
                "--allow-unverified",
                "fixture has no verification profile",
            ],
        )
        .status
        .success()
    );
}

#[test]
fn pidless_starting_task_is_not_released_after_supervisor_crash() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    // Recreate the ambiguous crash state exactly: a supervisor that died after
    // persisting the Starting record but before recording its child's pid. (The
    // previous version raced a real `kill -9 $PPID` against the supervisor's pid
    // write; whichever side won changed the observed state, so the test flaked.)
    let repo_bucket = std::fs::read_dir(cache.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record_path = repo_bucket.join(format!("{id}.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["commands"] = serde_json::json!([{
        "argv": ["sh", "-c", "sleep 1"],
        "pid": null,
        "process_start": null,
        "started_at": record["created_at"],
        "ended_at": null,
        "exit_code": null,
        "state": "starting",
    }]);
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();

    let stalled = wait_for(&repo, &cache, "stalled");
    assert!(stalled["tasks"][0]["commands"][0]["pid"].is_null());
    assert!(
        !run(&repo, &cache, &["task", "finish", "--task-id", &id])
            .status
            .success()
    );
}

#[cfg(unix)]
fn process_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
#[test]
fn exec_timeout_kills_the_whole_process_group_and_reports_124() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    // The command backgrounds a grandchild and writes its pid: group kill must
    // take the grandchild down too, not just the direct child.
    let output = run(
        &repo,
        &cache,
        &[
            "task",
            "exec",
            "--task-id",
            &id,
            "--timeout-secs",
            "1",
            "--",
            "sh",
            "-c",
            "sleep 30 & echo $! > grandchild.pid; wait",
        ],
    );
    assert_eq!(output.status.code(), Some(124));

    let grandchild = std::fs::read_to_string(repo.join("grandchild.pid")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(grandchild.trim()) {
        assert!(
            Instant::now() < deadline,
            "grandchild survived the group kill"
        );
        thread::sleep(Duration::from_millis(50));
    }

    let report = status(&repo, &cache);
    assert_eq!(report["tasks"][0]["status"], "failed");
    assert_eq!(report["tasks"][0]["commands"][0]["exit_code"], 124);
    assert_eq!(report["tasks"][0]["commands"][0]["state"], "interrupted");
}

#[cfg(unix)]
#[test]
fn exec_timeout_includes_waiting_for_strict_builder_admission() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    std::fs::write(
        repo.join(".grove.toml"),
        "governor_mode = 'strict'\ncpu_slots = 2\nmax_builders = 1\n",
    )
    .unwrap();
    let id = begin(&repo, &cache, "src");
    let holder = Command::new(GROVE)
        .args([
            "exec",
            "--tag",
            "holder",
            "--",
            "sh",
            "-c",
            "touch holder.started; sleep 3",
        ])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ready = Instant::now() + Duration::from_secs(5);
    while !repo.join("holder.started").exists() {
        assert!(
            Instant::now() < ready,
            "holder never acquired strict admission"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let started = Instant::now();
    let output = run(
        &repo,
        &cache,
        &[
            "task",
            "exec",
            "--task-id",
            &id,
            "--timeout-secs",
            "1",
            "--",
            "sh",
            "-c",
            "touch should-not-run",
        ],
    );

    assert_eq!(output.status.code(), Some(124));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!repo.join("should-not-run").exists());
    let holder_output = holder.wait_with_output().unwrap();
    assert!(
        holder_output.status.success(),
        "{}",
        String::from_utf8_lossy(&holder_output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn sigterm_to_task_exec_stops_the_child_and_records_143() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    let mut grove = Command::new(GROVE)
        .args(["task", "exec", "--task-id", &id, "--", "sleep", "30"])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let active = wait_for_active_command(&repo, &cache);
    let child_pid = active["tasks"][0]["commands"][0]["pid"].to_string();

    assert!(
        Command::new("kill")
            .args(["-TERM", &grove.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let exit = grove.wait().unwrap();
    assert_eq!(exit.code(), Some(143), "supervisor exits with 128+SIGTERM");

    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(&child_pid) {
        assert!(
            Instant::now() < deadline,
            "executor survived the forwarded signal"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let report = status(&repo, &cache);
    assert_eq!(report["tasks"][0]["commands"][0]["exit_code"], 143);
    assert_eq!(report["tasks"][0]["commands"][0]["state"], "interrupted");
}

#[cfg(unix)]
#[test]
fn sigint_to_task_exec_forwards_sigint_and_records_130() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    let mut grove = Command::new(GROVE)
        .args(["task", "exec", "--task-id", &id, "--", "sleep", "30"])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &cache)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_active_command(&repo, &cache);

    assert!(
        Command::new("kill")
            .args(["-INT", &grove.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let exit = grove.wait().unwrap();
    assert_eq!(exit.code(), Some(130), "supervisor exits with 128+SIGINT");
    let report = status(&repo, &cache);
    assert_eq!(report["tasks"][0]["commands"][0]["exit_code"], 130);
    assert_eq!(report["tasks"][0]["commands"][0]["state"], "interrupted");
}

#[test]
fn huge_timeout_does_not_panic_the_supervisor() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    let output = run(
        &repo,
        &cache,
        &[
            "task",
            "exec",
            "--task-id",
            &id,
            "--timeout-secs",
            "18446744073709551615",
            "--",
            "git",
            "--version",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn task_reap_migrates_a_schema_four_record_without_a_source_binding() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    let repo_bucket = std::fs::read_dir(cache.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record_path = repo_bucket.join(format!("{id}.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["schema_version"] = 4.into();
    record.as_object_mut().unwrap().remove("source_sha256");
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();

    let output = run(&repo, &cache, &["task", "reap", "--ttl", "0"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reaped: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reaped["reaped"][0]["id"], id);
    let migrated: Value = serde_json::from_slice(&std::fs::read(record_path).unwrap()).unwrap();
    assert_eq!(migrated["schema_version"], 5);
    assert!(migrated["source_sha256"].is_null());
}

#[test]
fn task_reap_directly_recovers_a_dead_supervisor_starting_record() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    let repo_bucket = std::fs::read_dir(cache.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record_path = repo_bucket.join(format!("{id}.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["commands"] = serde_json::json!([{
        "argv": ["sh", "-c", "sleep 1"],
        "pid": null,
        "process_start": null,
        "supervisor_pid": 99999999u32,
        "supervisor_start": 1,
        "started_at": record["created_at"],
        "ended_at": null,
        "exit_code": null,
        "state": "starting",
    }]);
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();

    // No status pass first: reap itself must see through the dead supervisor
    // instead of preserving the record as permanently ambiguous.
    let output = run(&repo, &cache, &["task", "reap", "--ttl", "0"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reaped: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reaped["reaped"][0]["id"], id, "{reaped}");
}

#[test]
fn dead_supervisor_starting_record_is_reconciled_not_wedged() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let id = begin(&repo, &cache, "src");

    // A Starting record whose supervisor is provably dead: the pid belongs to a
    // process that already exited, and the bogus start time defeats pid reuse.
    let exited = Command::new("true").spawn().unwrap().wait_with_output();
    assert!(exited.is_ok());
    let repo_bucket = std::fs::read_dir(cache.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record_path = repo_bucket.join(format!("{id}.json"));
    let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["commands"] = serde_json::json!([{
        "argv": ["sh", "-c", "sleep 1"],
        "pid": null,
        "process_start": null,
        "supervisor_pid": 99999999u32,
        "supervisor_start": 1,
        "started_at": record["created_at"],
        "ended_at": null,
        "exit_code": null,
        "state": "starting",
    }]);
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();

    // Unlike the identity-less record above, this one reconciles to interrupted
    // and the task can terminate normally instead of wedging forever.
    wait_for(&repo, &cache, "failed");
    assert!(
        run(
            &repo,
            &cache,
            &[
                "task",
                "finish",
                "--task-id",
                &id,
                "--allow-unverified",
                "fixture has no verification profile",
            ],
        )
        .status
        .success()
    );
}

#[test]
fn corrupt_registry_records_are_quarantined_not_fatal() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    begin(&repo, &cache, "src");
    let claimed = run(&repo, &cache, &["claim", "--agent", "bob", "docs"]);
    assert!(claimed.status.success());
    let registry = |kind: &str| {
        std::fs::read_dir(cache.join(kind))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    };
    let tasks = registry("tasks");
    let claims = registry("claims");
    std::fs::write(tasks.join("zzz.json"), "not json").unwrap();
    std::fs::write(claims.join("zzz.json"), "{\"agent\":3}").unwrap();

    begin(&repo, &cache, "tests");
    let granted = run(&repo, &cache, &["claim", "--agent", "carol", "benchmark"]);
    assert!(
        granted.status.success(),
        "{}",
        String::from_utf8_lossy(&granted.stderr)
    );
    for dir in [&tasks, &claims] {
        assert!(!dir.join("zzz.json").exists());
        assert!(dir.join("zzz.json.corrupt").exists());
    }
}
