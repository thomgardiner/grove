use super::{clear_worktree, copy_entry, materialize, restore_index};
use crate::snapshot::{self, Entry, Kind};
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "snapshot@example.test"]);
    git(dir, &["config", "user.name", "snapshot-test"]);
}

#[test]
fn materializes_the_captured_head_after_the_source_moves() {
    let base = tempdir().unwrap();
    let source = base.path().join("source");
    let root = base.path().join("cache");
    init(&source);
    fs::write(source.join("file"), "captured\n").unwrap();
    git(&source, &["add", "file"]);
    git(&source, &["commit", "-q", "-m", "captured"]);
    let start = snapshot::capture(&source).unwrap();
    let captured = start.head().unwrap().to_string();

    git(&source, &["commit", "--allow-empty", "-q", "-m", "later"]);
    let frozen = materialize(&root, &source, &start).unwrap();
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(frozen.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), captured);
}

#[test]
fn frozen_writes_ignore_a_swapped_worktree_path() {
    let base = tempdir().unwrap();
    let source = base.path().join("source");
    let frozen = base.path().join("frozen");
    let held = base.path().join("held");
    let victim = base.path().join("victim");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&frozen).unwrap();
    fs::create_dir(&victim).unwrap();
    fs::write(source.join("new"), b"new").unwrap();
    fs::write(frozen.join("stale"), b"stale").unwrap();
    fs::write(victim.join("sentinel"), b"keep").unwrap();
    let directory = File::open(&frozen).unwrap();
    fs::rename(&frozen, &held).unwrap();
    symlink(&victim, &frozen).unwrap();

    clear_worktree(&directory).unwrap();
    copy_entry(
        &source,
        &directory,
        &Entry {
            path: "new".into(),
            tracked: false,
            kind: Kind::File,
            sha256: None,
            mode: None,
        },
    )
    .unwrap();
    assert!(!held.join("stale").exists());
    assert_eq!(fs::read(held.join("new")).unwrap(), b"new");
    assert_eq!(fs::read(victim.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn restore_index_uses_the_held_worktree_directory() {
    let base = tempdir().unwrap();
    let source = base.path().join("source");
    let frozen = base.path().join("frozen");
    let held = base.path().join("held");
    let victim = base.path().join("victim");
    init(&source);
    fs::write(source.join("file"), "captured\n").unwrap();
    git(&source, &["add", "file"]);
    git(&source, &["commit", "-q", "-m", "captured"]);
    fs::write(source.join("file"), "staged\n").unwrap();
    git(&source, &["add", "file"]);
    let snapshot = snapshot::capture(&source).unwrap();
    git(
        &source,
        &["worktree", "add", "--detach", frozen.to_str().unwrap()],
    );
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("sentinel"), b"keep").unwrap();
    let directory = File::open(&frozen).unwrap();
    fs::rename(&frozen, &held).unwrap();
    symlink(&victim, &frozen).unwrap();

    restore_index(&source, &directory, &snapshot).unwrap();
    let index = Command::new("git")
        .args(["show", ":file"])
        .current_dir(&held)
        .output()
        .unwrap();
    assert!(index.status.success());
    assert_eq!(index.stdout, b"staged\n");
    assert_eq!(fs::read(victim.join("sentinel")).unwrap(), b"keep");
}
