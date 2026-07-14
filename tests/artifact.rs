use grove::{api::Grove, artifact};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn setup() -> (TempDir, TempDir, Grove) {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    (root, workspace, grove)
}

fn stage(grove: &Grove, tag: &str, path: &str, bytes: &[u8]) {
    let lane = grove.tagged_lane(tag).unwrap();
    let source = lane.dir.join(path);
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(source, bytes).unwrap();
}

#[test]
fn exports_a_lane_file_atomically_with_its_hash() {
    let (root, workspace, producer) = setup();
    stage(&producer, "release", "target/release/grove", b"artifact");
    drop(producer);
    let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
    let destination = workspace.path().join("bin/grove");

    let exported = artifact::export(
        &grove,
        "release",
        Path::new("target/release/grove"),
        &destination,
        false,
    )
    .unwrap();

    let mut hash = Sha256::new();
    hash.update(b"artifact");
    assert_eq!(fs::read(&destination).unwrap(), b"artifact");
    assert_eq!(exported.sha256, format!("{:x}", hash.finalize()));
    assert!(!exported.verified);
}

#[test]
fn missing_or_traversing_source_leaves_no_destination() {
    let (_root, workspace, grove) = setup();
    let missing = workspace.path().join("missing");
    let traversal = workspace.path().join("traversal");

    assert!(
        artifact::export(
            &grove,
            "release",
            Path::new("target/release/missing"),
            &missing,
            false,
        )
        .is_err()
    );
    assert!(
        artifact::export(
            &grove,
            "release",
            Path::new("../outside"),
            &traversal,
            false,
        )
        .is_err()
    );
    assert!(!missing.exists());
    assert!(!traversal.exists());
}

#[test]
fn existing_destination_is_preserved_on_failure() {
    let (_root, workspace, grove) = setup();
    stage(&grove, "release", "target/release/grove", b"new");
    let destination = workspace.path().join("grove");
    fs::write(&destination, b"old").unwrap();

    assert!(
        artifact::export(
            &grove,
            "release",
            Path::new("target/release/grove"),
            &destination,
            false,
        )
        .is_err()
    );
    assert_eq!(fs::read(destination).unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn symlink_source_cannot_escape_the_lane() {
    use std::os::unix::fs::symlink;

    let (_root, workspace, grove) = setup();
    let outside = workspace.path().join("outside");
    fs::write(&outside, b"not an artifact").unwrap();

    let lane = grove.tagged_lane("release").unwrap();
    let link = lane.dir.join("target/release/grove");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&outside, &link).unwrap();
    drop(lane);

    let destination = workspace.path().join("grove");
    assert!(
        artifact::export(
            &grove,
            "release",
            Path::new("target/release/grove"),
            &destination,
            false,
        )
        .is_err()
    );
    assert!(!destination.exists());
}
