use grove::api::Grove;
use serde_json::Value;
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

fn init(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "artifact@example.test"]);
    git(repo, &["config", "user.name", "artifact-test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='artifact_fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn run(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .unwrap()
}

#[test]
fn export_requires_evidence_or_a_durable_explicit_override() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let grove = Grove::with_root(cache.clone(), &repo);
    let lane = grove.tagged_lane("release").unwrap();
    let source = lane.dir.join("target/release/tool");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"artifact").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    drop(lane);

    let destination = base.path().join("dist/tool");
    let blocked = run(
        &repo,
        &cache,
        &[
            "artifact",
            "export",
            "--tag",
            "release",
            "target/release/tool",
            "--to",
            destination.to_str().unwrap(),
        ],
    );
    assert!(!blocked.status.success());
    assert!(!destination.parent().unwrap().exists());

    let output = run(
        &repo,
        &cache,
        &[
            "artifact",
            "export",
            "--tag",
            "release",
            "target/release/tool",
            "--to",
            destination.to_str().unwrap(),
            "--allow-unverified",
            "fixture exception",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let export: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(export["verified"], false);
    assert_eq!(export["override_reason"], "fixture exception");
    let audit_dir = cache.join("exports");
    let repo_dir = std::fs::read_dir(audit_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let audit = std::fs::read_dir(repo_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let audit: Value = serde_json::from_slice(&std::fs::read(audit).unwrap()).unwrap();
    assert_eq!(audit["published"], true);
    assert_eq!(audit["override_reason"], "fixture exception");
    assert_eq!(std::fs::read(&destination).unwrap(), b"artifact");
    // The export is byte-identical AND still executable: a published binary
    // that lost its mode is not the artifact that was verified.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "exported binary keeps its execute bits"
        );
    }
}
