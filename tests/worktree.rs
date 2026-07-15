//! Integration tests for the worktree pool against a real git repo: acquire creates
//! a leased worktree, reap salvages its work then removes it, and — the invariant
//! that makes reap safe to run unattended — reap never touches a worktree grove did
//! not create.

use grove::project;
use grove::worktree::{self, AcquireRequest};
use std::fs;
use std::path::{Path, PathBuf};
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

fn init_repo(at: &Path) -> PathBuf {
    fs::create_dir_all(at).unwrap();
    git(at, &["init", "-q"]);
    git(at, &["config", "user.email", "t@example.com"]);
    git(at, &["config", "user.name", "grove-test"]);
    fs::write(at.join("file"), "x").unwrap();
    // Pin the toolchain so acquire's derived lane key is deterministic regardless of
    // the test runner's RUSTUP_TOOLCHAIN.
    fs::write(
        at.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\n",
    )
    .unwrap();
    git(at, &["add", "."]);
    git(at, &["commit", "-q", "-m", "init"]);
    at.to_path_buf()
}

#[test]
fn acquire_leases_a_worktree_and_reap_salvages_then_removes_it() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: Some("grove/tester".into()),
        base: "HEAD".into(),
    };

    let worktree = worktree::acquire(&req).unwrap();
    assert!(worktree.exists(), "worktree checked out");
    assert!(worktree.join("file").exists(), "base commit is present");

    let leased = worktree::list(&root);
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].branch, "grove/tester");
    assert_eq!(leased[0].agent, "tester");

    // The agent leaves uncommitted work behind, then wanders off.
    fs::write(worktree.join("wip.txt"), "important unsaved work").unwrap();

    // ttl=0 => any idle worktree is abandoned. Reap must not lose the work.
    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert_eq!(report.reaped.len(), 1, "the abandoned worktree was reaped");
    assert_eq!(
        report.reaped[0].saved_to.as_deref(),
        Some("grove/tester"),
        "dirty work salvaged onto the lease branch"
    );
    assert!(!worktree.exists(), "the worktree directory is gone");

    // The salvaged work survives as a commit on the branch.
    let tree = git_out(&repo, &["ls-tree", "-r", "--name-only", "grove/tester"]);
    assert!(
        tree.contains("wip.txt"),
        "salvaged work committed to the branch, tree was: {tree}"
    );
    assert!(worktree::list(&root).is_empty(), "lease dropped");
}

#[test]
fn reap_never_touches_a_worktree_grove_did_not_create() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");

    // A worktree made by hand, with no grove lease.
    let manual = base.path().join("manual-wt");
    git(&repo, &["worktree", "add", "-q", manual.to_str().unwrap()]);
    assert!(manual.exists());

    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert!(report.reaped.is_empty(), "nothing reaped: no lease exists");
    assert!(manual.exists(), "the unmanaged worktree is untouched");
}

#[test]
fn reap_leaves_a_worktree_whose_lane_is_actively_building() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: Some("grove/tester".into()),
        base: "HEAD".into(),
    };
    let worktree = worktree::acquire(&req).unwrap();

    // Simulate a live build: hold the exact lane lock a build in this worktree takes.
    let ws = worktree.to_string_lossy().into_owned();
    let toolchain = project::toolchain(&worktree);
    let building = grove::cache::acquire(&root, &ws, &toolchain).unwrap();

    // ttl=0 => idle by the clock, but the held lane proves a build is live.
    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert!(report.reaped.is_empty(), "must not reap under a live build");
    assert_eq!(report.skipped.len(), 1);
    assert!(
        report.skipped[0].reason.contains("active build"),
        "unexpected skip reason: {}",
        report.skipped[0].reason
    );
    assert!(worktree.exists(), "worktree left intact");

    // Build finishes -> lock released -> reap reclaims it.
    drop(building);
    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert_eq!(report.reaped.len(), 1);
    assert!(!worktree.exists());
}

#[test]
fn release_refuses_live_lanes_and_switched_checkouts() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: Some("grove/tester".into()),
        base: "HEAD".into(),
    };
    let worktree = worktree::acquire(&req).unwrap();
    let ws = worktree.to_string_lossy().into_owned();

    // A live tagged lane (task exec / verify) blocks an explicit release.
    let executing = grove::cache::acquire_tagged(&root, &ws, "stable", "task-live").unwrap();
    let Err(err) = worktree::release(&root, &worktree) else {
        panic!("release must refuse under a live tagged lane");
    };
    assert!(err.to_string().contains("tagged lane"), "{err:#}");
    assert!(worktree.exists());
    drop(executing);

    // A checkout that left the leased branch is not ours to remove.
    git(&worktree, &["checkout", "-q", "-b", "someone-elses-work"]);
    let Err(err) = worktree::release(&root, &worktree) else {
        panic!("release must refuse a checkout that left the leased branch");
    };
    assert!(
        err.to_string()
            .contains("no longer grove's leased worktree"),
        "{err:#}"
    );
    assert!(worktree.exists());

    // Back on the leased branch, release proceeds.
    git(&worktree, &["checkout", "-q", "grove/tester"]);
    let outcome = worktree::release(&root, &worktree).unwrap();
    assert_eq!(outcome.branch, "grove/tester");
    assert!(!worktree.exists());
}

#[test]
fn reap_leaves_a_worktree_whose_tagged_lane_is_live() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: Some("grove/tester".into()),
        base: "HEAD".into(),
    };
    let worktree = worktree::acquire(&req).unwrap();

    // Simulate a live `task exec`/`verify`: those run in tagged lanes, which leave the
    // untagged build-lane lock free. Reap must still treat the worktree as owned.
    let ws = worktree.to_string_lossy().into_owned();
    let executing = grove::cache::acquire_tagged(&root, &ws, "stable", "task-live").unwrap();

    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert!(
        report.reaped.is_empty(),
        "must not reap under a live tagged lane"
    );
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("tagged lane"));
    assert!(worktree.exists(), "worktree left intact");

    // The tagged command finishes -> reap may reclaim it.
    drop(executing);
    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert_eq!(report.reaped.len(), 1);
    assert!(!worktree.exists());
}

#[test]
fn reap_quarantines_a_stale_lease_and_spares_the_reused_path() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: Some("grove/tester".into()),
        base: "HEAD".into(),
    };
    let worktree = worktree::acquire(&req).unwrap();

    // The checkout at this path is now something else — a human switched it off the
    // grove branch. The lease is stale; reap must not remove this checkout.
    git(&worktree, &["checkout", "-q", "-b", "someones-own-work"]);

    let report = worktree::reap(&root, &repo, 0, false).unwrap();
    assert!(
        report.reaped.is_empty(),
        "must not remove a checkout it no longer owns"
    );
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("stale lease"));
    assert!(worktree.exists(), "the reused checkout is spared");
    assert!(
        worktree::list(&root).is_empty(),
        "the stale lease was quarantined"
    );
}

#[test]
fn squash_collapses_a_branch_into_one_clean_commit() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: Some("grove/tester".into()),
        base: "HEAD".into(),
    };
    let worktree = worktree::acquire(&req).unwrap();

    // Three messy commits, like a swarm agent would leave behind.
    for i in 1..=3 {
        fs::write(worktree.join(format!("f{i}.txt")), format!("{i}")).unwrap();
        git(&worktree, &["add", "-A"]);
        git(&worktree, &["commit", "-q", "-m", &format!("wip {i}")]);
    }

    let out = worktree::squash(&root, &worktree, None, Some("clean: the feature")).unwrap();
    assert_eq!(out.squashed, 3, "collapsed all three commits");
    assert_eq!(out.message, "clean: the feature");

    // One commit beyond the base, carrying all three files.
    let beyond = git_out(&repo, &["rev-list", "--count", "HEAD..grove/tester"]);
    assert_eq!(beyond, "1", "exactly one commit beyond the base");
    let tree = git_out(&repo, &["ls-tree", "-r", "--name-only", "grove/tester"]);
    for i in 1..=3 {
        assert!(
            tree.contains(&format!("f{i}.txt")),
            "f{i}.txt survived the squash"
        );
    }
}

#[test]
fn squash_never_strands_the_branch_and_leaves_dirty_state_alone() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: Some("grove/tester".into()),
        base: "HEAD".into(),
    };
    let worktree = worktree::acquire(&req).unwrap();

    // A net-empty branch: add a file, then a commit that reverts it. The old
    // reset-then-commit squash failed halfway here and left the branch stripped
    // to its fork point.
    fs::write(worktree.join("f.txt"), "x").unwrap();
    git(&worktree, &["add", "-A"]);
    git(&worktree, &["commit", "-q", "-m", "add f"]);
    git(&worktree, &["rm", "-q", "f.txt"]);
    git(&worktree, &["commit", "-q", "-m", "remove f"]);

    // Staged and unstaged changes present during the squash must survive it untouched.
    fs::write(worktree.join("staged.txt"), "staged").unwrap();
    git(&worktree, &["add", "staged.txt"]);
    fs::write(worktree.join("unstaged.txt"), "unstaged").unwrap();

    let out = worktree::squash(&root, &worktree, None, None).unwrap();
    assert_eq!(out.squashed, 2, "both commits collapsed");
    assert_eq!(
        out.message, "add f",
        "oldest subject is the default message"
    );

    let beyond = git_out(&repo, &["rev-list", "--count", "HEAD..grove/tester"]);
    assert_eq!(
        beyond, "1",
        "the branch keeps exactly one (empty-diff) commit"
    );
    let status = git_out(&worktree, &["status", "--porcelain"]);
    assert!(
        status.contains("A  staged.txt") && status.contains("?? unstaged.txt"),
        "dirty state passes through the squash untouched, was: {status}"
    );
}

#[test]
fn dry_run_reports_without_removing() {
    let base = tempdir().unwrap();
    let repo = init_repo(&base.path().join("repo"));
    let root = base.path().join("cache");
    let req = AcquireRequest {
        root: &root,
        cwd: &repo,
        agent: "tester".into(),
        branch: None,
        base: "HEAD".into(),
    };
    let worktree = worktree::acquire(&req).unwrap();

    let report = worktree::reap(&root, &repo, 0, true).unwrap();
    assert!(report.dry_run);
    assert_eq!(report.reaped.len(), 1, "reported as reapable");
    assert!(worktree.exists(), "but left in place");
    assert_eq!(worktree::list(&root).len(), 1, "lease kept");
}
