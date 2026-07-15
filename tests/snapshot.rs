use grove::snapshot::{self, Kind};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn repo() -> tempfile::TempDir {
    let base = tempdir().unwrap();
    let dir = base.path();
    std::fs::create_dir_all(dir.join("dir")).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "snapshot@example.test"]);
    git(dir, &["config", "user.name", "snapshot-test"]);
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(dir.join("dir/file"), "tracked\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
    base
}

#[test]
fn includes_tracked_and_untracked_content_but_excludes_ignored_output() {
    let repo = repo();
    let before = snapshot::capture(repo.path()).unwrap();
    std::fs::create_dir_all(repo.path().join("target")).unwrap();
    std::fs::write(repo.path().join("target/ignored"), "ignored").unwrap();
    assert_eq!(
        before.sha256,
        snapshot::capture(repo.path()).unwrap().sha256
    );

    std::fs::write(repo.path().join("untracked"), "untracked").unwrap();
    let untracked = snapshot::capture(repo.path()).unwrap();
    assert_ne!(before.sha256, untracked.sha256);
    assert!(
        untracked
            .entries
            .iter()
            .any(|entry| !entry.tracked && entry.path == "untracked")
    );

    std::fs::remove_file(repo.path().join("dir/file")).unwrap();
    let deleted = snapshot::capture(repo.path()).unwrap();
    assert!(
        deleted
            .entries
            .iter()
            .any(|entry| entry.path == "dir/file" && entry.kind == Kind::Deleted)
    );
}

#[cfg(unix)]
#[test]
fn rejects_a_symlinked_parent_outside_the_workspace() {
    use std::os::unix::fs::symlink;

    let repo = repo();
    let outside = tempdir().unwrap();
    std::fs::write(outside.path().join("file"), "outside\n").unwrap();
    std::fs::remove_file(repo.path().join("dir/file")).unwrap();
    std::fs::remove_dir(repo.path().join("dir")).unwrap();
    symlink(outside.path(), repo.path().join("dir")).unwrap();
    assert!(snapshot::capture(repo.path()).is_err());
}

#[cfg(unix)]
#[test]
fn hashes_executable_mode_and_symlink_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let repo = repo();
    let before = snapshot::capture(repo.path()).unwrap();
    let mut permissions = std::fs::metadata(repo.path().join("dir/file"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(repo.path().join("dir/file"), permissions).unwrap();
    assert_ne!(
        before.sha256,
        snapshot::capture(repo.path()).unwrap().sha256
    );

    symlink("dir/file", repo.path().join("link")).unwrap();
    let first = snapshot::capture(repo.path()).unwrap();
    std::fs::remove_file(repo.path().join("link")).unwrap();
    symlink(".gitignore", repo.path().join("link")).unwrap();
    assert_ne!(first.sha256, snapshot::capture(repo.path()).unwrap().sha256);
}

#[test]
fn concurrent_identical_persists_share_a_valid_manifest() {
    let repo = repo();
    let root = tempdir().unwrap();
    let snapshot = snapshot::capture(repo.path()).unwrap();
    let reference = snapshot.reference();
    let root = root.path();
    let snapshot = &snapshot;
    let gate = Arc::new(Barrier::new(8));

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let gate = Arc::clone(&gate);
                scope.spawn(move || {
                    gate.wait();
                    snapshot::persist(root, "snapshot-concurrency", snapshot)
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap().unwrap() == reference);
        }
    });
    assert!(
        snapshot::validate(root, "snapshot-concurrency", &reference)
            .unwrap()
            .reference()
            == reference
    );
}
