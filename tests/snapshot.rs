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

fn git_text(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
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

#[test]
fn hashes_the_staged_index_even_when_working_bytes_stay_the_same() {
    let repo = repo();
    let path = repo.path().join("dir/file");
    std::fs::write(&path, "staged\n").unwrap();
    git(repo.path(), &["add", "dir/file"]);
    std::fs::write(&path, "working\n").unwrap();
    let staged = snapshot::capture(repo.path()).unwrap();

    git(repo.path(), &["reset", "-q", "HEAD", "--", "dir/file"]);
    let unstaged = snapshot::capture(repo.path()).unwrap();

    assert_eq!(std::fs::read(path).unwrap(), b"working\n");
    assert_ne!(staged.sha256, unstaged.sha256);
}

#[test]
fn changed_paths_includes_staged_only_changes() {
    let repo = repo();
    let before = snapshot::capture(repo.path()).unwrap();
    let path = repo.path().join("dir/file");
    std::fs::write(&path, "staged\n").unwrap();
    git(repo.path(), &["add", "dir/file"]);
    std::fs::write(&path, "tracked\n").unwrap();
    let after = snapshot::capture(repo.path()).unwrap();

    assert_eq!(
        snapshot::changed_paths(repo.path(), &before, &after).unwrap(),
        vec!["dir/file".to_string()]
    );
}

#[test]
fn hashes_head_even_when_index_and_working_bytes_stay_the_same() {
    let repo = repo();
    let before = snapshot::capture(repo.path()).unwrap();

    git(
        repo.path(),
        &["commit", "--allow-empty", "-q", "-m", "new head"],
    );
    let after = snapshot::capture(repo.path()).unwrap();

    assert_ne!(before.sha256, after.sha256);
}

#[test]
fn captures_an_uninitialized_gitlink_and_its_index_commit() {
    let repo = repo();
    let first = git_text(repo.path(), &["rev-parse", "HEAD"]);
    git(
        repo.path(),
        &["commit", "--allow-empty", "-q", "-m", "second commit"],
    );
    let second = git_text(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::create_dir_all(repo.path().join("deps/submodule")).unwrap();
    git(
        repo.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},deps/submodule"),
        ],
    );

    let before = snapshot::capture(repo.path()).unwrap();
    let gitlink = before
        .entries
        .iter()
        .find(|entry| entry.path == "deps/submodule")
        .unwrap();
    assert!(gitlink.tracked);
    assert!(gitlink.kind == Kind::File);
    assert_eq!(gitlink.mode, Some(0o160000));

    git(
        repo.path(),
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{second},deps/submodule"),
        ],
    );
    assert_ne!(
        before.sha256,
        snapshot::capture(repo.path()).unwrap().sha256
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

#[test]
fn concurrent_captures_share_the_git_index_safely() {
    let repo = repo();
    let workspace = repo.path().to_path_buf();
    let gate = Arc::new(Barrier::new(8));
    let snapshots = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let workspace = workspace.clone();
                scope.spawn(move || {
                    gate.wait();
                    snapshot::capture(&workspace)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect::<Vec<_>>()
    });

    assert!(snapshots.windows(2).all(|pair| pair[0] == pair[1]));
}
