use grove::cache;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tempfile::tempdir;

fn pressured_gc(root: &Path) -> cache::GcReport {
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("GROVE_MIN_FREE_GB", "1000000") };
    let report = cache::gc(root);
    unsafe { std::env::remove_var("GROVE_MIN_FREE_GB") };
    report
}

fn scratch(root: &Path, base: &str, name: &str) -> PathBuf {
    let path = root.join(base).join(name);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("artifact"), b"do not delete").unwrap();
    path
}

fn owner(path: &Path, pid: u32, started: u64) {
    let name = path.file_name().unwrap().to_string_lossy();
    write_owner(path, &name, pid, started);
}

fn write_owner(path: &Path, name: &str, pid: u32, started: u64) {
    let path_name = path.file_name().unwrap().to_string_lossy();
    let sidecar = path.with_file_name(format!("{path_name}.owner.json"));
    fs::write(
        sidecar,
        serde_json::to_vec(&json!({
            "schema": 1,
            "name": name,
            "pid": pid,
            "started": started,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn process_start(pid: u32) -> u64 {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).unwrap().start_time()
}

#[test]
fn malformed_lane_metadata_never_authorizes_lru_eviction() {
    let root = tempdir().unwrap();
    let lane = root.path().join("lanes/ambiguous");
    fs::create_dir_all(&lane).unwrap();
    fs::write(lane.join(".grove-meta.json"), b"{not-json").unwrap();
    fs::write(lane.join("artifact"), b"preserve").unwrap();

    let report = pressured_gc(root.path());

    assert!(lane.exists(), "unknown lane ownership must fail closed");
    assert!(!report.evicted.iter().any(|id| id == "ambiguous"));
}

#[test]
fn unreadable_lane_metadata_never_authorizes_lru_eviction() {
    let root = tempdir().unwrap();
    let lane = root.path().join("lanes/unreadable");
    fs::create_dir_all(lane.join(".grove-meta.json")).unwrap();
    fs::write(lane.join("artifact"), b"preserve").unwrap();

    let report = pressured_gc(root.path());

    assert!(lane.exists(), "unreadable ownership must fail closed");
    assert!(!report.evicted.iter().any(|id| id == "unreadable"));
}

#[test]
fn prefix_only_staging_and_backup_directories_are_preserved() {
    let root = tempdir().unwrap();
    let staging = scratch(root.path(), "lanes", ".grove-staging-4294967295-forged");
    let backup = scratch(root.path(), "canonical", ".grove-old-4294967295-forged");

    pressured_gc(root.path());

    assert!(staging.exists(), "a name is not cleanup authority");
    assert!(backup.exists(), "a name is not cleanup authority");
}

#[test]
fn malformed_staging_owner_is_preserved() {
    let root = tempdir().unwrap();
    let staging = scratch(root.path(), "lanes", ".grove-staging-malformed");
    let name = staging.file_name().unwrap().to_string_lossy();
    fs::write(
        staging.with_file_name(format!("{name}.owner.json")),
        b"{not-json",
    )
    .unwrap();

    pressured_gc(root.path());

    assert!(staging.exists(), "ambiguous ownership must fail closed");
}

#[test]
fn mismatched_staging_owner_is_preserved() {
    let root = tempdir().unwrap();
    let staging = scratch(root.path(), "lanes", ".grove-staging-mismatched");
    write_owner(&staging, ".grove-staging-somewhere-else", u32::MAX, 1);

    pressured_gc(root.path());

    assert!(
        staging.exists(),
        "the owner must name the exact scratch path"
    );
}

#[test]
fn matching_live_staging_owner_is_preserved() {
    let root = tempdir().unwrap();
    let staging = scratch(root.path(), "lanes", ".grove-staging-live");
    let pid = std::process::id();
    owner(&staging, pid, process_start(pid));

    pressured_gc(root.path());

    assert!(staging.exists(), "live scratch must remain untouched");
}

#[test]
fn matching_dead_staging_owner_is_reclaimed() {
    let root = tempdir().unwrap();
    let staging = scratch(root.path(), "lanes", ".grove-staging-dead");
    owner(&staging, u32::MAX, 1);

    let report = pressured_gc(root.path());

    assert!(!staging.exists(), "dead authenticated scratch is garbage");
    assert!(
        report
            .reclaimed
            .iter()
            .any(|item| item == "staging:.grove-staging-dead")
    );
}
