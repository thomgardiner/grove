use super::*;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[test]
fn locks_are_namespaced_by_repository() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let a = path(root.path(), "repo-a", workspace.path()).unwrap();
    let b = path(root.path(), "repo-b", workspace.path()).unwrap();

    assert_ne!(a, b);
}

#[test]
fn exclusive_waits_for_a_local_shared_holder() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let workspace = fs::canonicalize(workspace.path()).unwrap();
    let repo = crate::project::repo_identity(&workspace);
    let shared = shared(root.path(), &workspace).unwrap();
    let cache = root.path().to_path_buf();
    let path = workspace;
    let (tx, rx) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        let guard = exclusive(&cache, &repo, &path).unwrap();
        tx.send(()).unwrap();
        guard
    });
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    drop(shared);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(thread.join().unwrap());
}

#[test]
fn lane_start_waits_until_exclusive_cleanup_finishes() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let workspace = fs::canonicalize(workspace.path()).unwrap();
    let repo = crate::project::repo_identity(&workspace);
    let exclusive = exclusive(root.path(), &repo, &workspace).unwrap();
    let cache = root.path().to_path_buf();
    let path = workspace.to_string_lossy().into_owned();
    let (tx, rx) = mpsc::channel();

    let thread = std::thread::spawn(move || {
        let lane = crate::cache::acquire(&cache, &path, "stable").unwrap();
        tx.send(()).unwrap();
        lane
    });
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    drop(exclusive);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(thread.join().unwrap());
}

#[test]
fn try_lane_returns_while_exclusive_cleanup_is_held() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let workspace = fs::canonicalize(workspace.path()).unwrap();
    let repo = crate::project::repo_identity(&workspace);
    let _exclusive = exclusive(root.path(), &repo, &workspace).unwrap();

    let started = Instant::now();
    let lane =
        crate::cache::try_acquire(root.path(), workspace.to_str().unwrap(), "stable").unwrap();

    assert!(lane.is_none());
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[test]
fn shared_deadline_can_cancel_while_exclusive_cleanup_is_held() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let workspace = fs::canonicalize(workspace.path()).unwrap();
    let repo = crate::project::repo_identity(&workspace);
    let _exclusive = exclusive(root.path(), &repo, &workspace).unwrap();
    let started = Instant::now();

    let guard = shared_until(root.path(), &workspace, &|| {
        started.elapsed() >= Duration::from_millis(50)
    })
    .unwrap();

    assert!(guard.is_none());
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn absent_planned_path_keeps_the_same_identity_after_creation() {
    let root = tempdir().unwrap();
    let parent = tempdir().unwrap();
    let workspace = parent.path().join("agent-worktree");
    let before = path(root.path(), "repo", &workspace).unwrap();
    fs::create_dir(&workspace).unwrap();
    let after = path(root.path(), "repo", &workspace).unwrap();

    assert_eq!(before, after);
}
