use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn git(repo: &std::path::Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn linked_worktrees_never_share_an_unverified_bootstrap_lane() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let linked_path = base.path().join("linked");
    let root = base.path().join("cache");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("Cargo.toml"), "[workspace]\nresolver='3'\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "grove@example.test"]);
    git(&repo, &["config", "user.name", "Grove Test"]);
    git(&repo, &["add", "Cargo.toml"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    assert!(
        Command::new("git")
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&linked_path)
            .arg("HEAD")
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );

    let primary = grove::api::Grove::with_root(root.clone(), &repo);
    let linked = grove::api::Grove::with_root(root, &linked_path);
    assert_eq!(primary.repo(), linked.repo());
    let expected =
        [primary.workspace(), linked.workspace()].map(|path| path.to_string_lossy().into_owned());
    let primary_lane = primary.bootstrap_lane().unwrap().dir.clone();
    drop(primary);
    let linked_lane = linked.bootstrap_lane().unwrap().dir.clone();

    assert_ne!(primary_lane, linked_lane);
    for (lane, workspace) in [primary_lane, linked_lane].iter().zip(expected) {
        let meta: serde_json::Value =
            serde_json::from_slice(&fs::read(lane.join(".grove-meta.json")).unwrap()).unwrap();
        assert_eq!(meta["workspace"], workspace);
    }
}
