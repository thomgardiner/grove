//! Integration tests for the `Grove` facade against real temp directories. An empty temp
//! dir is not a cargo project, so `project::workspace` falls back to the dir itself and no
//! git repo is needed — enough to exercise the resolve → canonical → seed wiring.

use grove::api::Grove;
use grove::cache;
use std::fs;
use tempfile::tempdir;

#[test]
fn facade_resolves_the_workspace_and_seeds_a_lane_from_the_canonical() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), ws.path());

    // The workspace is the resolved (symlinks-followed) path, matching what prewarm keys.
    assert_eq!(grove.workspace(), cache::canonical_path(ws.path()));

    // Put an artifact in the canonical this facade resolves, then seed a lane from it.
    let canonical = grove.canonical();
    fs::create_dir_all(canonical.join("target")).unwrap();
    fs::write(canonical.join("target/libengine.rmeta"), b"seed").unwrap();

    let lane = grove.seeded_lane().unwrap();
    assert_eq!(
        fs::read(lane.target_dir.join("libengine.rmeta")).unwrap(),
        b"seed",
        "seeded_lane clones the canonical into the lane"
    );
}

#[test]
fn facade_promotes_a_lane_into_the_canonical() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), ws.path());

    let lane = grove.lane().unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("final.rlib"), b"built").unwrap();

    grove.promote(&lane).unwrap();

    assert_eq!(
        fs::read(grove.canonical().join("target/final.rlib")).unwrap(),
        b"built",
        "promote publishes the lane as the canonical"
    );
}
