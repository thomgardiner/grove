#![cfg(unix)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use grove::api::Grove;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn init(repo: &Path, command: &str) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "release@example.test"]);
    git(repo, &["config", "user.name", "release-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='release_fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn present() {}\n").unwrap();
    std::fs::write(
        repo.join(".grove.toml"),
        format!(
            "[verification]\nrequired = [\"release\"]\n\n[verification.profiles.release]\ncontinue_on_failure = false\ncommands = [{{ argv = [\"sh\", \"-c\", '{command}'], allow_zero_tests = false }}]\n"
        ),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn run(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .env("GROVE_RELEASE_SIGNING_KEY", STANDARD.encode([7u8; 32]))
        .output()
        .unwrap()
}

fn begin(repo: &Path, cache: &Path) -> String {
    let output = run(
        repo,
        cache,
        &[
            "task", "begin", "--agent", "alice", "--task", "release", "--scope", "src",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn freeze_signs_hashed_executable_artifacts_from_the_verified_lane() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    let bundle = base.path().join("bundle");
    init(
        &repo,
        "test -z \"$GROVE_RELEASE_SIGNING_KEY\" && mkdir -p \"$CARGO_TARGET_DIR/release\" && printf bundle > \"$CARGO_TARGET_DIR/release/tool\" && chmod 755 \"$CARGO_TARGET_DIR/release/tool\"",
    );
    let id = begin(&repo, &cache);
    let output = run(
        &repo,
        &cache,
        &[
            "release",
            "freeze",
            "--task-id",
            &id,
            "--profile",
            "release",
            "--artifact",
            "target/release/tool",
            "--out",
            bundle.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let manifest_bytes = std::fs::read(bundle.join("manifest.json")).unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let manifest_text = String::from_utf8(manifest_bytes.clone()).unwrap();
    assert_eq!(manifest["task_id"], id);
    assert_eq!(
        manifest["snapshot_manifest"]["sha256"],
        manifest["snapshot"]["sha256"]
    );
    assert!(manifest["snapshot_manifest"]["entries"].is_array());
    assert!(manifest.get("repository").is_none());
    assert!(manifest["receipts"][0].get("stdout_tail").is_none());
    assert!(manifest["receipts"][0].get("lane").is_none());
    assert!(
        manifest["receipts"][0]["lane_tag"]
            .as_str()
            .is_some_and(|tag| tag.starts_with("release-freeze-"))
    );
    assert!(!manifest_text.contains(repo.to_str().unwrap()));
    assert_eq!(
        manifest["artifacts"][0]["sha256"],
        report["artifacts"][0]["sha256"]
    );
    assert_eq!(
        std::fs::read(bundle.join("target/release/tool")).unwrap(),
        b"bundle"
    );
    assert_ne!(
        std::fs::metadata(bundle.join("target/release/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );

    let public: [u8; 32] = STANDARD
        .decode(manifest["signer_public_key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let signature: [u8; 64] = STANDARD
        .decode(
            std::fs::read_to_string(bundle.join("manifest.sig"))
                .unwrap()
                .trim(),
        )
        .unwrap()
        .try_into()
        .unwrap();
    let key = VerifyingKey::from_bytes(&public).unwrap();
    let signature = Signature::from_bytes(&signature);
    assert!(key.verify_strict(&manifest_bytes, &signature).is_ok());
    assert!(key.verify_strict(b"tampered manifest", &signature).is_err());
}

#[test]
fn freeze_refuses_workspace_drift_without_publishing_a_bundle() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    let bundle = base.path().join("bundle");
    init(&repo, "printf changed > src/lib.rs");
    let id = begin(&repo, &cache);
    let output = run(
        &repo,
        &cache,
        &[
            "release",
            "freeze",
            "--task-id",
            &id,
            "--profile",
            "release",
            "--artifact",
            "target/release/tool",
            "--out",
            bundle.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(!bundle.exists());
    assert_eq!(
        std::fs::read(repo.join("src/lib.rs")).unwrap(),
        b"pub fn present() {}\n"
    );
}

#[test]
fn freeze_never_signs_an_artifact_left_in_a_reusable_verify_lane() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    let bundle = base.path().join("bundle");
    init(&repo, "true");
    let id = begin(&repo, &cache);
    let lane = Grove::with_root(cache.clone(), &repo)
        .tagged_lane("verify-release")
        .unwrap();
    std::fs::create_dir_all(lane.target_dir.join("release")).unwrap();
    std::fs::write(lane.target_dir.join("release/tool"), b"stale").unwrap();
    drop(lane);

    let output = run(
        &repo,
        &cache,
        &[
            "release",
            "freeze",
            "--task-id",
            &id,
            "--profile",
            "release",
            "--artifact",
            "target/release/tool",
            "--out",
            bundle.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(!bundle.exists());
}

#[test]
fn freeze_builds_the_captured_staged_and_untracked_content() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    let bundle = base.path().join("bundle");
    init(
        &repo,
        "grep -Fx \"pub fn dirty() {}\" src/lib.rs && test \"$(git show :src/lib.rs)\" = \"pub fn staged() {}\" && test \"$(cat note)\" = note && mkdir -p \"$CARGO_TARGET_DIR/release\" && printf bundle > \"$CARGO_TARGET_DIR/release/tool\"",
    );
    std::fs::write(repo.join("src/lib.rs"), "pub fn staged() {}\n").unwrap();
    git(&repo, &["add", "src/lib.rs"]);
    std::fs::write(repo.join("src/lib.rs"), "pub fn dirty() {}\n").unwrap();
    std::fs::write(repo.join("note"), "note\n").unwrap();
    let id = begin(&repo, &cache);

    let output = run(
        &repo,
        &cache,
        &[
            "release",
            "freeze",
            "--task-id",
            &id,
            "--profile",
            "release",
            "--artifact",
            "target/release/tool",
            "--out",
            bundle.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(repo.join("src/lib.rs")).unwrap(),
        b"pub fn dirty() {}\n"
    );
    assert_eq!(std::fs::read(repo.join("note")).unwrap(), b"note\n");
    assert_eq!(
        std::fs::read(bundle.join("target/release/tool")).unwrap(),
        b"bundle"
    );
}

#[test]
fn freeze_never_cleans_a_replaced_frozen_worktree() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    let bundle = base.path().join("bundle");
    let victim = base.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    let sentinel = victim.join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    let command = format!(
        "mv \"$PWD\" \"$PWD-held\" && ln -s \"{}\" \"$PWD\" && printf swapped > \"{}/swap\"",
        victim.display(),
        victim.display(),
    );
    init(&repo, &command);
    let id = begin(&repo, &cache);

    let output = run(
        &repo,
        &cache,
        &[
            "release",
            "freeze",
            "--task-id",
            &id,
            "--profile",
            "release",
            "--artifact",
            "target/release/tool",
            "--out",
            bundle.to_str().unwrap(),
        ],
    );

    assert!(!output.status.success());
    assert!(!bundle.exists());
    assert_eq!(std::fs::read(victim.join("swap")).unwrap(), b"swapped");
    assert_eq!(std::fs::read(sentinel).unwrap(), b"keep");
}
