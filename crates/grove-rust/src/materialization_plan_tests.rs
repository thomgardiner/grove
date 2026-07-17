use super::*;
use crate::git;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn fingerprint() -> Fingerprint {
    Fingerprint {
        hash: "cargo-fingerprint".into(),
        value: Value::Null,
    }
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn commit(repo: &Path) -> String {
    if repo.join("Cargo.toml").is_file() {
        lock(repo);
    }
    git::run(repo, &["init", "-q"]).unwrap();
    git::run(repo, &["config", "user.email", "planner@example.invalid"]).unwrap();
    git::run(repo, &["config", "user.name", "Planner Test"]).unwrap();
    git::run(repo, &["add", "-A"]).unwrap();
    git::run(repo, &["commit", "-q", "-m", "fixture"]).unwrap();
    git::capture(repo, &["rev-parse", "HEAD"]).unwrap()
}

fn lock(workspace: &Path) {
    cargo_metadata::MetadataCommand::new()
        .current_dir(workspace)
        .exec()
        .unwrap();
}

fn package(repo: &Path, name: &str, manifest: &str) {
    write(repo, &format!("crates/{name}/Cargo.toml"), manifest);
    write(
        repo,
        &format!("crates/{name}/src/lib.rs"),
        "pub fn value() {}\n",
    );
}

fn workspace() -> (TempDir, String) {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    package(
        repo.path(),
        "a",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n\
         [dependencies]\nb={path='../b'}\nmac={path='../mac'}\n\
         [dev-dependencies]\ndev={path='../dev'}\n\
         [build-dependencies]\nbuild={path='../build'}\n",
    );
    for name in ["b", "build", "dev"] {
        package(
            repo.path(),
            name,
            &format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n"),
        );
    }
    package(
        repo.path(),
        "mac",
        "[package]\nname='mac'\nversion='0.1.0'\nedition='2024'\n\
         [lib]\nproc-macro=true\n",
    );
    package(
        repo.path(),
        "c",
        "[package]\nname='c'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(
        repo.path(),
        "crates/c/fixtures/large.bin",
        &"x".repeat(4096),
    );
    write(
        repo.path(),
        ".cargo/config.toml",
        "[build]\nincremental=true\n",
    );
    write(repo.path(), "schemas/model.json", "{}\n");
    let base = commit(repo.path());
    (repo, base)
}

fn sourced_package(template: &cargo_metadata::Package, manifest: &str) -> cargo_metadata::Package {
    // Source IDs exclude a package from the index whatever its location —
    // both true registry crates and vendored directory-source replacements.
    let mut fake = serde_json::to_value(template).unwrap();
    fake["name"] = "adler2".into();
    fake["id"] =
        format!("registry+https://github.com/rust-lang/crates.io-index#adler2@{manifest}").into();
    fake["source"] = "registry+https://github.com/rust-lang/crates.io-index".into();
    fake["manifest_path"] = manifest.into();
    serde_json::from_value(fake).unwrap()
}

/// Inclusion is the manifest's real location, not index membership. Registry
/// dependencies live outside the repository and are skipped (found in the
/// wild: any repo with one registry dependency failed to plan). Vendored
/// directory-source packages carry source IDs but live IN the repo, so their
/// manifests must still exist at the selected base.
#[test]
fn registry_packages_skip_verification_but_vendored_in_repo_ones_do_not() {
    let (repo, base) = workspace();
    let mut metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(repo.path())
        .exec()
        .unwrap();
    let template = metadata.packages[0].clone();
    // A registry dependency under a cargo home outside the repository:
    // skipped, never demanded from the Git tree. The manifest exists on disk
    // (cargo metadata only reports manifests it has read).
    let cargo_home = TempDir::new().unwrap();
    write(
        cargo_home.path(),
        "registry/src/adler2-2.0.1/Cargo.toml",
        "[package]\nname='adler2'\nversion='2.0.1'\nedition='2024'\n",
    );
    metadata.packages.push(sourced_package(
        &template,
        cargo_home
            .path()
            .join("registry/src/adler2-2.0.1/Cargo.toml")
            .to_str()
            .unwrap(),
    ));
    // A vendored package whose manifest is committed: verified and present.
    let vendored_committed = repo.path().join("crates/c/Cargo.toml");
    metadata.packages.push(sourced_package(
        &template,
        vendored_committed.to_str().unwrap(),
    ));
    let tree = Tree::load(repo.path(), &base).unwrap();
    verify_inputs(&metadata, &tree).unwrap();

    // A vendored manifest on disk but absent from the base could smuggle
    // metadata past verification; it must be refused, not skipped.
    write(
        repo.path(),
        "vendor/adler2/Cargo.toml",
        "[package]\nname='adler2'\nversion='2.0.1'\nedition='2024'\n",
    );
    let uncommitted = repo.path().join("vendor/adler2/Cargo.toml");
    metadata
        .packages
        .push(sourced_package(&template, uncommitted.to_str().unwrap()));
    let error = verify_inputs(&metadata, &tree).unwrap_err().to_string();
    assert!(
        error.contains("not present at the selected base"),
        "{error}"
    );
}

fn plan_scope(repo: &Path, base: &str, scope: &str) -> MaterializationPlan {
    plan(PlanInput {
        workspace: repo,
        base_oid: base,
        scopes: &[scope.into()],
        extras: &[],
        config: None,
        fingerprint: &fingerprint(),
        planned_at: 1,
    })
    .unwrap()
}

#[test]
fn paths_resolve_and_cone_metrics_match_git_semantics() {
    let tree = Tree::new(vec![
        Entry::blob("Cargo.toml", 10),
        Entry::blob("README.md", 3),
        Entry::blob("crates/Cargo.toml", 5),
        Entry::blob("crates/a/Cargo.toml", 7),
        Entry::measured("crates/a/src/lib.rs", 20, 25),
        Entry::blob("crates/a/fixtures/large.bin", 100),
    ]);
    assert_eq!(tree.cone("crates/a").unwrap(), "crates/a");
    assert_eq!(tree.cone("crates/a/src/lib.rs").unwrap(), "crates/a/src");
    assert_eq!(tree.cone("crates/a/new/file.rs").unwrap(), "crates/a");
    assert_eq!(tree.cone("Cargo.toml").unwrap(), ".");
    for invalid in ["/absolute", "../escape", "C:\\absolute"] {
        assert!(normalize_scope(invalid).is_err());
    }
    assert_eq!(
        tree.metrics(&["crates/a/src".into()]).unwrap(),
        Metrics {
            tracked_files: 5,
            git_blob_bytes: 45,
            working_files: 5,
            working_logical_bytes: 50,
        }
    );
}

#[test]
fn no_reduction_falls_back_to_full() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "src/lib.rs", "pub fn root() {}\n");
    let base = commit(repo.path());
    let no_reduction = plan_scope(repo.path(), &base, "src");
    assert_eq!(
        no_reduction.fallback_reason,
        Some(FallbackReason::NoReduction)
    );
}

#[test]
fn planner_rejects_a_dirty_or_different_source() {
    let (repo, base) = workspace();
    write(repo.path(), "Cargo.toml", "dirty\n");
    let scopes = vec!["crate:a".into()];
    let error = plan(PlanInput {
        workspace: repo.path(),
        base_oid: &base,
        scopes: &scopes,
        extras: &[],
        config: None,
        fingerprint: &fingerprint(),
        planned_at: 1,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("clean"), "{error}");

    git::run(repo.path(), &["checkout", "--", "Cargo.toml"]).unwrap();
    let error = plan(PlanInput {
        workspace: repo.path(),
        base_oid: "0000000000000000000000000000000000000000",
        scopes: &scopes,
        extras: &[],
        config: None,
        fingerprint: &fingerprint(),
        planned_at: 1,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("HEAD"), "{error}");

    for (base_oid, planned_at) in [("", 1), (&base, 0)] {
        let error = plan(PlanInput {
            workspace: repo.path(),
            base_oid,
            scopes: &scopes,
            extras: &[],
            config: None,
            fingerprint: &fingerprint(),
            planned_at,
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("requires"), "{error}");
    }
}

#[test]
fn planner_selects_dependency_closure_support_and_exact_savings() {
    let (repo, _) = workspace();
    let scopes = vec!["crate:a".into()];
    let extras = vec!["schemas".into()];
    let config = repo.path().join(".grove.toml");
    write(
        repo.path(),
        ".grove.toml",
        "[worktree]\nmaterialize=['schemas']\n",
    );
    git::run(repo.path(), &["add", ".grove.toml"]).unwrap();
    git::run(repo.path(), &["commit", "-q", "-m", "config"]).unwrap();
    let base = git::capture(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    let plan = plan(PlanInput {
        workspace: repo.path(),
        base_oid: &base,
        scopes: &scopes,
        extras: &extras,
        config: Some(&config),
        fingerprint: &fingerprint(),
        planned_at: 1,
    })
    .unwrap();
    assert_eq!(plan.mode, MaterializationMode::Sparse);
    assert_eq!(plan.closure_packages, ["a", "b", "build", "dev", "mac"]);
    assert_eq!(
        plan.closure_cones,
        [
            "crates/a",
            "crates/b",
            "crates/build",
            "crates/dev",
            "crates/mac"
        ]
    );
    assert_eq!(plan.support_cones, [".cargo", "crates/c/src", "schemas"]);
    assert_eq!(plan.full_tracked_files - plan.selected_tracked_files, 1);
    assert_eq!(
        plan.full_git_blob_bytes - plan.selected_git_blob_bytes,
        4096
    );
    assert_eq!(plan.full_working_files - plan.selected_working_files, 1);
    assert_eq!(
        plan.full_working_logical_bytes - plan.selected_working_logical_bytes,
        4096
    );
    plan.validate().unwrap();
}

#[test]
fn requested_root_package_falls_back_to_full() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "src/lib.rs", "pub fn root() {}\n");
    let base = commit(repo.path());
    let scopes = vec!["crate:root".into()];
    let plan = plan(PlanInput {
        workspace: repo.path(),
        base_oid: &base,
        scopes: &scopes,
        extras: &[],
        config: None,
        fingerprint: &fingerprint(),
        planned_at: 1,
    })
    .unwrap();
    assert_eq!(plan.mode, MaterializationMode::Full);
    assert_eq!(plan.fallback_reason, Some(FallbackReason::RootScope));
    plan.validate().unwrap();
}

#[path = "materialization_plan_integration_tests.rs"]
mod integration;
