use super::*;
use crate::api::Grove;
use std::fs;
use std::sync::mpsc;
use std::time::Duration;
use tempfile::tempdir;

fn policy(keep_debuginfo: bool) -> Policy {
    Policy {
        keep_debuginfo,
        require_cow: false,
        governor: crate::config::Governor::best_effort(1),
        min_free_gb: Some(0),
        max_canonical_gb: None,
    }
}

fn warm_regular(grove: &Grove) -> PathBuf {
    let lane = grove.lane().unwrap();
    let dir = lane.dir.clone();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("warm.rlib"), b"warm").unwrap();
    drop(lane);
    dir
}

fn successful_bootstrap(grove: &Grove) -> PathBuf {
    let lane = grove.bootstrap_lane().unwrap();
    let dir = lane.dir.clone();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("built.rlib"), b"built").unwrap();
    succeed(&lane).unwrap();
    drop(lane);
    dir
}

#[test]
fn partial_bootstrap_without_success_keeps_the_regular_lane() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let regular = warm_regular(&grove);
    let lane = grove.bootstrap_lane().unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("partial.rlib"), b"partial").unwrap();
    drop(lane);

    grove.gc();

    assert!(regular.exists(), "partial output is not retention evidence");
}

#[test]
fn nonzero_command_clears_previous_bootstrap_success() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let regular = warm_regular(&grove);
    successful_bootstrap(&grove);
    let lane = grove.bootstrap_lane().unwrap();
    fs::write(lane.target_dir.join("failed.rlib"), b"failed").unwrap();
    drop(lane); // a nonzero command does not call `succeed`

    grove.gc();

    assert!(
        regular.exists(),
        "failed mutation invalidates earlier success"
    );
}

#[test]
fn killed_command_cannot_publish_bootstrap_success() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let regular = warm_regular(&grove);
    let lane = grove.bootstrap_lane().unwrap();
    fs::create_dir_all(&lane.build_dir).unwrap();
    fs::write(lane.build_dir.join("interrupted.o"), b"partial").unwrap();
    drop(lane); // process death drops the lock without calling `succeed`

    grove.gc();

    assert!(
        regular.exists(),
        "interrupted output is not retention evidence"
    );
}

#[test]
fn successful_bootstrap_reclaims_only_matching_regular_lanes() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let regular = warm_regular(&grove);
    let bootstrap = successful_bootstrap(&grove);

    grove.gc();

    assert!(
        !regular.exists(),
        "successful shared output replaces cold output"
    );
    assert!(
        bootstrap.exists(),
        "the successful bootstrap remains available"
    );
}

#[test]
fn empty_fake_canonical_does_not_replace_the_bootstrap() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let bootstrap = successful_bootstrap(&grove);
    fs::create_dir_all(grove.canonical()).unwrap();

    grove.gc();

    assert!(
        !grove.published(),
        "directory presence is not publication evidence"
    );
    assert!(
        bootstrap.exists(),
        "an unauthoritative canonical replaces nothing"
    );
    assert_eq!(grove.seeded_lane().unwrap().dir, bootstrap);
}

#[test]
fn empty_fake_canonical_does_not_seed_a_tagged_lane() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    fs::create_dir_all(grove.canonical().join("target")).unwrap();
    fs::write(grove.canonical().join("target/fake.rlib"), b"fake").unwrap();
    let bootstrap = grove.bootstrap_lane().unwrap().dir.clone();

    let lane = grove.seeded_tagged_lane("verify").unwrap();

    assert_eq!(
        lane.dir, bootstrap,
        "tagged callers use the same bootstrap fallback"
    );
    assert!(!lane.target_dir.join("fake.rlib").exists());
}

#[test]
fn tagged_seed_rechecks_publication_after_waiting_for_repromotion() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let source = grove.tagged_lane("canonical").unwrap();
    fs::create_dir_all(&source.target_dir).unwrap();
    fs::write(source.target_dir.join("old.rlib"), b"old").unwrap();
    grove.promote(&source).unwrap();
    drop(source);
    let bootstrap = grove.bootstrap_lane().unwrap().dir.clone();
    let canonical = grove.canonical();
    let lock = canonical_lock(root.path(), &canonical).unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    super::retention::unpublish(root.path(), &canonical).unwrap();

    let lane = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| grove.seeded_tagged_lane("verify").unwrap());
        drop(lock);
        waiting.join().unwrap()
    });

    assert_eq!(lane.dir, bootstrap);
    assert!(!lane.target_dir.join("old.rlib").exists());
}

#[test]
fn promoted_canonical_reclaims_the_bootstrap() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let bootstrap = successful_bootstrap(&grove);
    let lane = grove.lane().unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("canonical.rlib"), b"built").unwrap();
    grove.promote(&lane).unwrap();
    drop(lane);

    grove.gc();

    assert!(grove.published());
    assert!(
        !bootstrap.exists(),
        "published canonical replaces its bootstrap"
    );
}

#[test]
fn bootstrap_reclaim_holds_canonical_authority_lock_until_delete() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let bootstrap = successful_bootstrap(&grove);
    let source = grove.tagged_lane("canonical").unwrap();
    fs::create_dir_all(&source.target_dir).unwrap();
    grove.promote(&source).unwrap();
    drop(source);
    let canonical = grove.canonical();
    let cache = root.path().to_path_buf();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    std::thread::scope(|scope| {
        let collector = scope.spawn(move || {
            super::retention::reclaim_after(&cache, || {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let lock = canonical_lock(root.path(), &canonical).unwrap();
        assert!(
            fs2::FileExt::try_lock_exclusive(&lock).is_err(),
            "re-promotion cannot invalidate authority during bootstrap deletion"
        );
        release_tx.send(()).unwrap();
        let reclaimed = collector.join().unwrap();
        assert!(reclaimed.iter().any(|id| bootstrap.ends_with(id)));
    });
}

#[test]
fn failed_repromotion_revokes_old_publication_evidence() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let lane = grove.lane().unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("canonical.rlib"), b"built").unwrap();
    grove.promote(&lane).unwrap();
    assert!(grove.published());
    fs::remove_dir_all(&lane.dir).unwrap();

    assert!(grove.promote(&lane).is_err());

    assert!(
        !grove.published(),
        "a failed replacement cannot retain stale authority"
    );
}

#[test]
fn orphaned_publication_cannot_authorize_a_recreated_empty_canonical() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let lane = grove.lane().unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    grove.promote(&lane).unwrap();
    assert!(grove.published());
    fs::remove_dir_all(grove.canonical()).unwrap();
    fs::create_dir_all(grove.canonical()).unwrap();

    assert!(
        !grove.published(),
        "external metadata alone cannot authorize replacement contents"
    );
}

#[test]
fn policy_a_to_b_to_a_reuses_each_policy_identity() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let workspace = workspace.path().to_string_lossy().into_owned();
    let a = policy(false);
    let b = policy(true);
    let first_a = acquire_bootstrap_with_policy(root.path(), &workspace, "stable", &a).unwrap();
    let first_a_dir = first_a.dir.clone();
    succeed(&first_a).unwrap();
    drop(first_a);
    let lane_b = acquire_bootstrap_with_policy(root.path(), &workspace, "stable", &b).unwrap();
    let lane_b_dir = lane_b.dir.clone();
    succeed(&lane_b).unwrap();
    drop(lane_b);

    gc_with_policy(root.path(), &b);
    let second_a = acquire_bootstrap_with_policy(root.path(), &workspace, "stable", &a).unwrap();

    assert_eq!(second_a.dir, first_a_dir);
    assert!(
        lane_b_dir.exists(),
        "policy B is not a duplicate of policy A"
    );
}

#[test]
fn live_lane_lock_blocks_retention() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let regular = grove.lane().unwrap();
    let regular_dir = regular.dir.clone();
    successful_bootstrap(&grove);

    grove.gc();
    assert!(regular_dir.exists(), "retention cannot delete a held lane");
    drop(regular);
    grove.gc();
    assert!(
        !regular_dir.exists(),
        "the unlocked redundant lane is reclaimed"
    );
}

#[test]
fn live_bootstrap_lock_blocks_use_of_its_success_marker() {
    let root = tempdir().unwrap();
    let workspace = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let regular = warm_regular(&grove);
    let bootstrap = grove.bootstrap_lane().unwrap();
    fs::create_dir_all(&bootstrap.target_dir).unwrap();
    fs::write(bootstrap.target_dir.join("built.rlib"), b"built").unwrap();
    succeed(&bootstrap).unwrap();

    grove.gc();
    assert!(regular.exists(), "live evidence cannot authorize deletion");
    drop(bootstrap);
    grove.gc();
    assert!(
        !regular.exists(),
        "unlocked success can replace the cold lane"
    );
}
