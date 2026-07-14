//! Integration tests for the cache and copy-on-write seeding, against real temp
//! directories (no mocks). The clone benchmark is `#[ignore]`d; run it against a real
//! target with `GROVE_BENCH_SRC=/path/to/target cargo test --release bench -- --ignored --nocapture`.

use grove::{cache, seed};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn clone_tree_reproduces_the_source_and_replaces_the_destination() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    fs::create_dir_all(src.join("a/b")).unwrap();
    fs::write(src.join("a/b/deep.txt"), b"deep").unwrap();
    fs::write(src.join("top.txt"), b"top").unwrap();
    // Pre-existing (stale) destination content must be gone after cloning.
    fs::create_dir_all(&dst).unwrap();
    fs::write(dst.join("stale.txt"), b"stale").unwrap();

    seed::clone_tree(&src, &dst).unwrap();

    assert_eq!(fs::read(dst.join("a/b/deep.txt")).unwrap(), b"deep");
    assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
    assert!(
        !dst.join("stale.txt").exists(),
        "stale destination content must be replaced"
    );
}

// APFS is copy-on-write, so a strict clone succeeds on the dev/reference machine. On a
// non-CoW volume the same call is expected to fail rather than fall back to a full copy;
// that path is filesystem-specific and not asserted here.
#[cfg(target_os = "macos")]
#[test]
fn strict_cow_clone_succeeds_on_apfs() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("final.rlib"), b"artifact").unwrap();

    seed::clone_tree_cow(&src, &dst, true).unwrap();

    assert_eq!(fs::read(dst.join("final.rlib")).unwrap(), b"artifact");
}

#[test]
fn lane_ids_are_stable_and_specific() {
    assert_eq!(
        cache::lane_id("/repo", "stable"),
        cache::lane_id("/repo", "stable")
    );
    assert_ne!(
        cache::lane_id("/repo/a", "stable"),
        cache::lane_id("/repo/b", "stable")
    );
    assert_ne!(
        cache::lane_id("/repo", "stable"),
        cache::lane_id("/repo", "nightly")
    );
}

#[test]
fn seed_clones_a_cold_lane_and_leaves_a_warm_one_alone() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let ws_str = ws.path().to_string_lossy().into_owned();

    // A canonical holding one target artifact.
    let canonical = cache::canonical_dir(root.path(), &ws_str, "stable");
    fs::create_dir_all(canonical.join("target")).unwrap();
    fs::write(canonical.join("target/libengine.rmeta"), b"seed").unwrap();

    let lane = cache::acquire(root.path(), &ws_str, "stable").unwrap();
    assert!(!lane.target_dir.exists(), "a fresh lane is cold");
    assert!(
        cache::seed(root.path(), &lane, &canonical).unwrap(),
        "a cold lane with a canonical seeds"
    );
    assert_eq!(
        fs::read(lane.target_dir.join("libengine.rmeta")).unwrap(),
        b"seed"
    );

    assert!(
        !cache::seed(root.path(), &lane, &canonical).unwrap(),
        "a warm lane is left untouched"
    );
}

#[test]
fn promote_captures_the_whole_lane_into_the_canonical() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let ws_str = ws.path().to_string_lossy().into_owned();

    let lane = cache::acquire(root.path(), &ws_str, "stable").unwrap();
    fs::create_dir_all(&lane.build_dir).unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.build_dir.join("intermediate.o"), b"obj").unwrap();
    fs::write(lane.target_dir.join("final.rlib"), b"lib").unwrap();

    let canonical = cache::canonical_dir(root.path(), &ws_str, "stable");
    cache::promote(root.path(), &lane, &canonical).unwrap();

    assert_eq!(
        fs::read(canonical.join("build/intermediate.o")).unwrap(),
        b"obj"
    );
    assert_eq!(
        fs::read(canonical.join("target/final.rlib")).unwrap(),
        b"lib"
    );
}

#[test]
fn reclaim_stale_drops_gone_worktrees_and_keeps_live_ones() {
    let root = tempdir().unwrap();
    let live_ws = tempdir().unwrap();
    let live_str = live_ws.path().to_string_lossy().into_owned();
    let gone_str = root
        .path()
        .join("deleted-worktree")
        .to_string_lossy()
        .into_owned();

    let live = cache::acquire(root.path(), &live_str, "stable").unwrap();
    let gone = cache::acquire(root.path(), &gone_str, "stable").unwrap();
    let (live_dir, gone_dir) = (live.dir.clone(), gone.dir.clone());
    drop(live);
    drop(gone); // release locks so GC can claim them

    let reclaimed = cache::reclaim_stale(root.path());

    assert!(live_dir.exists(), "a live worktree's lane is kept");
    assert!(!gone_dir.exists(), "a gone worktree's lane is reclaimed");
    assert_eq!(reclaimed.len(), 1);
}

#[test]
#[ignore = "benchmark; needs GROVE_BENCH_SRC pointing at a real target dir on the same volume"]
fn bench_clone_large_tree() {
    let src = std::env::var("GROVE_BENCH_SRC").expect("set GROVE_BENCH_SRC");
    let dst = format!("{src}-grove-bench");
    let _ = fs::remove_dir_all(&dst);
    let started = std::time::Instant::now();
    seed::clone_tree(Path::new(&src), Path::new(&dst)).unwrap();
    let elapsed = started.elapsed();
    let _ = fs::remove_dir_all(&dst);
    eprintln!("clone_tree of {src} took {elapsed:?}");
}
