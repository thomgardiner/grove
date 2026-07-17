#![allow(clippy::unwrap_used)]

use grove::materialization_cargo::{self, capture, equivalent};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    source: PathBuf,
    candidate: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let source = root.path().join("full source α");
        write_workspace(&source);
        run(&source, &["cargo", "generate-lockfile"]);
        git(&source, &["init", "-q"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "Test"]);
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "fixture"]);
        let candidate = root.path().join("sparse candidate β");
        let source_arg = source.to_string_lossy();
        let candidate_arg = candidate.to_string_lossy();
        run(
            root.path(),
            &["git", "clone", "-q", &source_arg, &candidate_arg],
        );
        fs::remove_file(candidate.join("crates/unrelated/assets/payload.bin")).unwrap();
        Self {
            _root: root,
            source,
            candidate,
        }
    }

    fn fingerprints(
        &self,
    ) -> (
        materialization_cargo::Fingerprint,
        materialization_cargo::Fingerprint,
    ) {
        (
            capture(&self.source, &self.source).unwrap(),
            capture(&self.candidate, &self.candidate).unwrap(),
        )
    }
}

#[test]
fn checkout_paths_and_absent_unrelated_payload_are_equivalent() {
    let fixture = Fixture::new();
    let (source, candidate) = fixture.fingerprints();
    assert_eq!(source.value, candidate.value);
    assert_eq!(source.hash, candidate.hash);
    assert!(equivalent(&source, &candidate).unwrap());
}

#[test]
fn graph_and_tracked_config_changes_are_not_equivalent() {
    let fixture = Fixture::new();
    let source = capture(&fixture.source, &fixture.source).unwrap();
    let manifest = fixture.candidate.join("crates/app/Cargo.toml");
    let changed = fs::read_to_string(&manifest)
        .unwrap()
        .replace("cfg(unix)", "cfg(windows)");
    fs::write(&manifest, changed).unwrap();
    let graph = capture(&fixture.candidate, &fixture.candidate).unwrap();
    assert!(!equivalent(&source, &graph).unwrap());

    fs::copy(fixture.source.join("crates/app/Cargo.toml"), &manifest).unwrap();
    fs::write(
        fixture.candidate.join("config/shared.toml"),
        "[net]\nretry = 3\n",
    )
    .unwrap();
    let config = capture(&fixture.candidate, &fixture.candidate).unwrap();
    assert!(!equivalent(&source, &config).unwrap());
}

#[test]
fn declaration_order_does_not_change_the_fingerprint() {
    let fixture = Fixture::new();
    let source = capture(&fixture.source, &fixture.source).unwrap();
    let manifest = fixture.candidate.join("crates/app/Cargo.toml");
    let changed = fs::read_to_string(&manifest).unwrap().replace(
        "renamed = { package = 'shared', path = '../shared' }\nmacros = { path = '../macros' }",
        "macros = { path = '../macros' }\nrenamed = { package = 'shared', path = '../shared' }",
    );
    fs::write(manifest, changed).unwrap();
    let candidate = capture(&fixture.candidate, &fixture.candidate).unwrap();
    assert!(equivalent(&source, &candidate).unwrap());
}

#[test]
fn metadata_and_untracked_config_failures_are_closed() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.candidate.join("Cargo.lock")).unwrap();
    let error = capture(&fixture.candidate, &fixture.candidate)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("cargo metadata failed"));

    fs::copy(
        fixture.source.join("Cargo.lock"),
        fixture.candidate.join("Cargo.lock"),
    )
    .unwrap();
    fs::write(
        fixture.candidate.join(".cargo/config.toml"),
        "include = ['../config/shared.toml', '../config/untracked.toml']\n",
    )
    .unwrap();
    fs::write(
        fixture.candidate.join("config/untracked.toml"),
        "[net]\nretry = 4\n",
    )
    .unwrap();
    let error = capture(&fixture.candidate, &fixture.candidate)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("is not tracked"));
}

#[test]
fn optional_missing_config_is_accepted() {
    let fixture = Fixture::new();
    fs::write(
        fixture.candidate.join(".cargo/config.toml"),
        "include = [\n  '../config/shared.toml',\n  { path = '../config/missing.toml', optional = true },\n]\n",
    )
    .unwrap();
    capture(&fixture.candidate, &fixture.candidate).unwrap();
}

#[test]
fn workspace_outside_repository_is_rejected() {
    let fixture = Fixture::new();
    let other = TempDir::new().unwrap();
    let error = capture(&fixture.source, other.path())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("outside the Git repository"));
}

#[test]
fn unknown_checkout_path_field_fails_closed() {
    let fixture = Fixture::new();
    let (mut source, mut candidate) = fixture.fingerprints();
    insert_future_path(&mut source.value, &fixture.source);
    insert_future_path(&mut candidate.value, &fixture.candidate);
    let error = equivalent(&source, &candidate).unwrap_err().to_string();
    assert!(error.contains("unknown path-shaped metadata difference"));
}

#[test]
fn unknown_non_path_difference_is_a_metadata_mismatch() {
    let fixture = Fixture::new();
    let (mut source, mut candidate) = fixture.fingerprints();
    source.value["cargo"]["future_mode"] = Value::String("source".into());
    candidate.value["cargo"]["future_mode"] = Value::String("candidate".into());
    assert!(!equivalent(&source, &candidate).unwrap());
}

fn insert_future_path(value: &mut Value, root: &Path) {
    value["cargo"]["future_path"] = Value::String(
        root.join("future/input.json")
            .to_string_lossy()
            .into_owned(),
    );
}

fn write_workspace(root: &Path) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = ['crates/*']\ndefault-members = ['crates/app']\nresolver = '2'\n",
    );
    write(
        root,
        ".cargo/config.toml",
        "include = ['../config/shared.toml']\n[build]\njobs = 1\n",
    );
    write(root, "config/shared.toml", "[net]\nretry = 2\n");
    write(
        root,
        ".grove.toml",
        "[worktree]\nmaterialize = ['schemas']\n",
    );
    package(
        root,
        "app",
        "[dependencies]\nrenamed = { package = 'shared', path = '../shared' }\n\
         macros = { path = '../macros' }\n\
         [dev-dependencies]\ntestkit = { path = '../testkit' }\n\
         [build-dependencies]\nbuilder = { path = '../builder' }\n\
         [target.'cfg(unix)'.dependencies]\nplatform = { path = '../platform' }\n\
         [[bin]]\nname = 'app'\npath = 'app/main.rs'\n",
    );
    write(root, "crates/app/app/main.rs", "fn main() {}\n");
    for name in ["shared", "testkit", "builder", "platform", "unrelated"] {
        package(root, name, "");
    }
    write(
        root,
        "crates/macros/Cargo.toml",
        "[package]\nname = 'macros'\nversion = '0.1.0'\nedition = '2024'\n\
         [lib]\nproc-macro = true\n",
    );
    write(root, "crates/macros/src/lib.rs", "");
    write(
        root,
        "crates/unrelated/assets/payload.bin",
        &"payload".repeat(1024),
    );
}

fn package(root: &Path, name: &str, extra: &str) {
    write(
        root,
        &format!("crates/{name}/Cargo.toml"),
        &format!("[package]\nname = '{name}'\nversion = '0.1.0'\nedition = '2024'\n{extra}"),
    );
    write(
        root,
        &format!("crates/{name}/src/lib.rs"),
        "pub fn marker() {}\n",
    );
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let mut command = vec!["git"];
    command.extend_from_slice(args);
    run(root, &command);
}

fn run(root: &Path, args: &[&str]) {
    let output = Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
