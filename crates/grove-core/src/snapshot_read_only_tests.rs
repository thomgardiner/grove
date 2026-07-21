use super::*;
use std::process::Stdio;
use tempfile::tempdir;

#[test]
fn failed_capture_cleans_scratch_and_preserves_source_index() {
    let workspace = tempdir().unwrap();
    git(workspace.path(), &["init"]);
    git(
        workspace.path(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    git(workspace.path(), &["config", "user.name", "Grove Test"]);
    git(
        workspace.path(),
        &["config", "user.email", "grove@example.invalid"],
    );
    fs::write(workspace.path().join("conflict"), "base").unwrap();
    git(workspace.path(), &["add", "."]);
    git(workspace.path(), &["commit", "-m", "base"]);
    git(workspace.path(), &["checkout", "-b", "side"]);
    commit(workspace.path(), "side");
    git(workspace.path(), &["checkout", "main"]);
    commit(workspace.path(), "main");
    assert!(
        !Command::new("git")
            .args(["merge", "side"])
            .current_dir(workspace.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );

    let index = git_path(workspace.path(), "index").unwrap();
    let before = fs::read(&index).unwrap();
    let id = SCRATCH_ID.load(Ordering::Relaxed);
    let scratch = std::env::temp_dir().join(format!(
        "grove-snapshot-read-only-{}-{id}",
        std::process::id()
    ));
    assert!(index_tree(workspace.path()).is_err());
    assert_eq!(fs::read(index).unwrap(), before);
    assert!(!scratch.exists());
}

fn commit(workspace: &Path, value: &str) {
    fs::write(workspace.join("conflict"), value).unwrap();
    git(workspace, &["commit", "-am", value]);
}

fn git(workspace: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
