//! Integration test for prewarm: a real git repo with two worktrees, seeded from a
//! synthetic canonical. Also pins the symlink-resolution contract — prewarm and a
//! build must key the same lane for the same worktree.

use grove::{cache, project, watch};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn prewarm_seeds_every_worktree_from_the_canonical() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "grove-test"]);
    fs::write(repo.join("file"), "x").unwrap();
    // Pin the toolchain so prewarm's per-worktree derivation matches the canonical
    // this test creates, regardless of the runner's RUSTUP_TOOLCHAIN.
    fs::write(
        repo.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let worktree = base.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", worktree.to_str().unwrap()],
    );

    let root = base.path().join("cache");
    // Repo identity exactly as the watcher derives it: git-common-dir onto the workspace.
    let repo_dir = cache::canonical_path(&repo);
    let toolchain = project::toolchain(&repo_dir);
    let repo_id = repo_dir
        .join(git_out(&repo, &["rev-parse", "--git-common-dir"]))
        .to_string_lossy()
        .into_owned();

    let grove = grove::api::Grove::with_root(root.clone(), &repo_dir);
    let source = grove.tagged_lane("watch-fixture").unwrap();
    fs::create_dir_all(&source.target_dir).unwrap();
    fs::write(source.target_dir.join("libx.rmeta"), b"warm").unwrap();
    grove.promote(&source).unwrap();
    drop(source);

    let seeded = watch::prewarm(&root, &repo_dir, &repo_id).unwrap();
    assert_eq!(
        seeded.len(),
        2,
        "both worktrees should seed, got {seeded:?}"
    );

    for workspace in [repo_dir, cache::canonical_path(&worktree)] {
        let id = cache::lane_id(&workspace.to_string_lossy(), &toolchain);
        let artifact = root.join("lanes").join(&id).join("target/libx.rmeta");
        assert_eq!(
            fs::read(&artifact).unwrap(),
            b"warm",
            "the lane for {} was seeded",
            workspace.display()
        );
    }
}

#[test]
fn prewarm_skips_an_unpublished_canonical_directory() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "grove-test"]);
    fs::write(repo.join("file"), "x").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let root = base.path().join("cache");
    let repo_dir = cache::canonical_path(&repo);
    let toolchain = project::toolchain(&repo_dir);
    let repo_id = project::repo_identity(&repo_dir);
    let canonical = cache::canonical_dir(&root, &repo_id, &toolchain);
    fs::create_dir_all(canonical.join("target")).unwrap();
    fs::write(canonical.join("target/fake.rmeta"), b"fake").unwrap();

    let seeded = watch::prewarm(&root, &repo_dir, &repo_id).unwrap();

    assert!(seeded.is_empty());
    let id = cache::lane_id(&repo_dir.to_string_lossy(), &toolchain);
    assert!(
        !root
            .join("lanes")
            .join(id)
            .join("target/fake.rmeta")
            .exists()
    );
}
