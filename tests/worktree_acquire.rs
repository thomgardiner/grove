//! Crash-boundary recovery for worktree acquisition intents.

use grove::cache;
use grove::project;
use grove::worktree::{self, AcquireRequest};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) {
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
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn init_repo(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "grove-test"]);
    fs::write(path.join("file"), "x").unwrap();
    fs::write(
        path.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "init"]);
    fs::canonicalize(path).unwrap()
}

fn intent_path(root: &Path, repo: &str, workspace: &Path) -> PathBuf {
    root.join("acquisitions")
        .join(cache::repo_slug(repo))
        .join(format!(
            "{}.json",
            cache::repo_slug(&workspace.to_string_lossy())
        ))
}

fn write_intent(root: &Path, repo: &Path, workspace: &Path, branch: &str) -> PathBuf {
    let repo_id = project::repo_identity(repo);
    let path = intent_path(root, &repo_id, workspace);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "repo": repo_id,
            "main_worktree": repo,
            "workspace": workspace,
            "branch": branch,
            "agent": "interrupted",
            "base_oid": git_out(repo, &["rev-parse", "HEAD"]),
            "created_at": 1
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn intent_without_a_worktree_is_removed_but_its_branch_is_preserved() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let workspace = base.path().join("never-created");
    git(&repo, &["branch", "grove/interrupted"]);
    let intent = write_intent(&root, &repo, &workspace, "grove/interrupted");

    worktree::reap(&root, &repo, u64::MAX, false).unwrap();

    assert!(!intent.exists());
    assert!(!git_out(&repo, &["rev-parse", "grove/interrupted"]).is_empty());
}

#[test]
fn exact_post_git_interruption_is_adopted_as_a_lease() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let workspace = base.path().join("interrupted");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "grove/interrupted",
            workspace.to_str().unwrap(),
            "HEAD",
        ],
    );
    let workspace = fs::canonicalize(workspace).unwrap();
    let intent = write_intent(&root, &repo, &workspace, "grove/interrupted");

    worktree::reap(&root, &repo, u64::MAX, false).unwrap();

    assert!(!intent.exists());
    let leases = worktree::list(&root);
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].path, workspace.to_string_lossy());
    assert_eq!(leases[0].branch, "grove/interrupted");
}

#[test]
fn dry_run_preserves_an_exact_interrupted_acquisition() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let workspace = base.path().join("interrupted");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "grove/interrupted",
            workspace.to_str().unwrap(),
            "HEAD",
        ],
    );
    let workspace = fs::canonicalize(workspace).unwrap();
    let intent = write_intent(&root, &repo, &workspace, "grove/interrupted");

    worktree::reap(&root, &repo, u64::MAX, true).unwrap();

    assert!(intent.exists());
    assert!(workspace.exists());
    assert!(worktree::list(&root).is_empty());
}

#[test]
fn mismatched_branch_preserves_the_intent_and_worktree() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let workspace = base.path().join("mismatch");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "grove/actual",
            workspace.to_str().unwrap(),
            "HEAD",
        ],
    );
    let workspace = fs::canonicalize(workspace).unwrap();
    let intent = write_intent(&root, &repo, &workspace, "grove/expected");

    worktree::reap(&root, &repo, u64::MAX, false).unwrap();

    assert!(intent.exists());
    assert!(workspace.exists());
    assert!(worktree::list(&root).is_empty());
}

#[test]
fn reused_non_worktree_path_preserves_the_intent_and_contents() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let workspace = base.path().join("reused");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("human.txt"), "keep").unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let intent = write_intent(&root, &repo, &workspace, "grove/interrupted");

    worktree::reap(&root, &repo, u64::MAX, false).unwrap();

    assert!(intent.exists());
    assert_eq!(
        fs::read_to_string(workspace.join("human.txt")).unwrap(),
        "keep"
    );
    assert!(worktree::list(&root).is_empty());
}

#[test]
fn malformed_intent_blocks_reconciliation_and_stays_on_disk() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let path = root
        .join("acquisitions")
        .join(cache::repo_slug(&project::repo_identity(&repo)))
        .join("broken.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{not-json").unwrap();

    let error = worktree::reap(&root, &repo, u64::MAX, false)
        .err()
        .unwrap()
        .to_string();

    assert!(error.contains("preserved for inspection"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), b"{not-json");
}

#[test]
fn normal_acquire_leaves_one_lease_and_no_intent() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let request = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "agent".into(),
        branch: Some("grove/agent".into()),
        base: "HEAD".into(),
    };

    let workspace = worktree::acquire(&request).unwrap();

    assert!(workspace.exists());
    assert_eq!(worktree::list(&root).len(), 1);
    let repo_intents = root
        .join("acquisitions")
        .join(cache::repo_slug(&project::repo_identity(&repo)));
    assert_eq!(fs::read_dir(repo_intents).into_iter().flatten().count(), 0);
}
