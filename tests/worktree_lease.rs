//! Durable worktree lease compatibility and renewal.

use grove::{cache, worktree};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn lease_path(root: &Path, workspace: &str) -> PathBuf {
    root.join("leases")
        .join(format!("{}.json", cache::lane_id(workspace, "stable")))
}

fn write_lease(root: &Path, workspace: &str, last_activity: Option<u64>) -> PathBuf {
    fs::create_dir_all(root.join("leases")).unwrap();
    let mut lease = serde_json::json!({
        "workspace": workspace,
        "branch": "grove/agent",
        "agent": "agent",
        "toolchain": "stable",
        "repo": "/repo/.git",
        "created_at": 1,
        "base_oid": "abc"
    });
    if let Some(last_activity) = last_activity {
        lease["last_activity"] = last_activity.into();
    }
    let path = lease_path(root, workspace);
    fs::write(&path, serde_json::to_vec_pretty(&lease).unwrap()).unwrap();
    path
}

#[test]
fn legacy_lease_without_last_activity_is_renewed_atomically() {
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let workspace = base.path().join("worktree");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let workspace_str = workspace.to_string_lossy();
    let path = write_lease(&root, &workspace_str, None);

    let before = now_secs();
    let lease = worktree::heartbeat(&root, &workspace).unwrap();
    assert!(lease.last_activity >= before);
    assert!(lease.last_activity > lease.created_at);

    let persisted: worktree::Lease = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(persisted.last_activity, lease.last_activity);
    assert!(worktree::list(&root)[0].idle_secs <= 1);
}

#[test]
fn heartbeat_refuses_unknown_and_duplicate_paths() {
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let workspace = base.path().join("worktree");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();

    let error = worktree::heartbeat(&root, &workspace)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("no grove lease"), "{error}");

    let workspace_str = workspace.to_string_lossy();
    write_lease(&root, &workspace_str, Some(2));
    let duplicate = root.join("leases/duplicate.json");
    fs::copy(lease_path(&root, &workspace_str), &duplicate).unwrap();
    let error = worktree::heartbeat(&root, &workspace)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("multiple grove leases"), "{error}");
}

#[test]
fn malformed_lease_is_preserved_and_never_treated_as_authority() {
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let workspace = base.path().join("worktree");
    fs::create_dir_all(root.join("leases")).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let malformed = root.join("leases/malformed.json");
    fs::write(&malformed, b"{not-json").unwrap();

    assert!(worktree::list(&root).is_empty());
    assert!(worktree::heartbeat(&root, &workspace).is_err());
    assert_eq!(fs::read(&malformed).unwrap(), b"{not-json");
}
