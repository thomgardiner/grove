use grove::{cache, doctor};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

#[cfg(windows)]
fn command_workspace(path: &Path) -> PathBuf {
    PathBuf::from(format!(r"\\?\{}", path.display()))
}

#[cfg(not(windows))]
fn command_workspace(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[test]
fn reports_local_linker_and_incremental_provenance() {
    let workspace = tempdir().unwrap();
    fs::create_dir_all(workspace.path().join(".cargo")).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        r#"
[package]
name = "doctor_fixture"
version = "0.1.0"
edition = "2024"

[profile.release]
incremental = false

[profile.fast]
inherits = "release"
opt-level = "s"
"#,
    )
    .unwrap();
    fs::write(
        workspace.path().join(".cargo/config.toml"),
        r#"
[target.x86_64-unknown-linux-gnu]
linker = "mold"

[target.'cfg(unix)']
rustflags = ["-C", "linker=clang"]

[profile.fast]
incremental = false
"#,
    )
    .unwrap();

    let report = doctor::report(workspace.path()).unwrap();

    assert!(!report.mold.repository_default_linker);
    assert!(report.mold.linker_settings.iter().any(|setting| {
        setting.scope == "target.x86_64-unknown-linux-gnu.linker" && setting.linker == "mold"
    }));
    assert!(report.mold.linker_settings.iter().any(|setting| {
        setting.scope == "target.cfg(unix).rustflags" && setting.linker == "clang"
    }));
    let release = report
        .incremental
        .disabled_profiles
        .iter()
        .find(|profile| profile.profile == "release")
        .unwrap();
    assert_eq!(release.incremental_source.source, "Cargo.toml");
    let fast = report
        .incremental
        .disabled_profiles
        .iter()
        .find(|profile| profile.profile == "fast")
        .unwrap();
    assert_eq!(fast.incremental_source.source, ".cargo/config.toml");
    assert_eq!(fast.opt_level, "s");
    assert_eq!(report.watchlist.len(), 4);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["mold"]["repository_default_linker"], false);
}

#[test]
fn reports_linker_settings_from_repository_includes() {
    let workspace = tempdir().unwrap();
    let cargo = workspace.path().join(".cargo");
    fs::create_dir_all(&cargo).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(cargo.join("config.toml"), "include = [\"linker.toml\"]\n").unwrap();
    fs::write(
        cargo.join("linker.toml"),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"mold\"\n",
    )
    .unwrap();

    let report = doctor::report(workspace.path()).unwrap();

    assert!(report.mold.linker_settings.iter().any(|setting| {
        setting.source == ".cargo/config.toml.include-0" && setting.linker == "mold"
    }));
}

#[test]
fn verbatim_workspace_and_plain_cargo_home_load_the_same_config_once() {
    let root = tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let cargo_home = root.path().join(".cargo");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&cargo_home).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        cargo_home.join("config.toml"),
        "[net]\ngit-fetch-with-cli = true\n",
    )
    .unwrap();

    let command_workspace = command_workspace(&workspace);
    #[cfg(windows)]
    {
        assert_ne!(command_workspace, workspace);
        assert_eq!(
            fs::canonicalize(&command_workspace).unwrap(),
            fs::canonicalize(&workspace).unwrap()
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_grove"))
        .arg("doctor")
        .current_dir(command_workspace)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn recursive_cargo_config_include_is_still_rejected() {
    let workspace = tempdir().unwrap();
    let cargo = workspace.path().join(".cargo");
    fs::create_dir_all(&cargo).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(cargo.join("config.toml"), "include = [\"shared.toml\"]\n").unwrap();
    fs::write(cargo.join("shared.toml"), "include = [\"config.toml\"]\n").unwrap();

    let error = doctor::report(workspace.path())
        .err()
        .expect("recursive include is rejected")
        .to_string();

    assert!(error.contains("Cargo config include cycle"), "{error}");
}

#[cfg(unix)]
#[test]
fn resolved_config_source_changes_lane_identity() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let cargo = workspace.join(".cargo");
    fs::create_dir_all(&cargo).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let first = root.path().join("first.toml");
    let second = root.path().join("second.toml");
    fs::write(&first, "[build]\nincremental = true\n").unwrap();
    fs::write(&second, "[build]\nincremental = true\n").unwrap();
    let config = cargo.join("config.toml");
    symlink(&first, &config).unwrap();
    let first_identity = doctor::report(&workspace)
        .unwrap()
        .incremental
        .identity_sha256;

    fs::remove_file(&config).unwrap();
    symlink(&second, &config).unwrap();
    let second_identity = doctor::report(&workspace)
        .unwrap()
        .incremental
        .identity_sha256;

    assert_ne!(first_identity, second_identity);
}

#[test]
fn favors_the_legacy_local_cargo_config_when_both_exist() {
    let workspace = tempdir().unwrap();
    let cargo = workspace.path().join(".cargo");
    fs::create_dir_all(&cargo).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(cargo.join("config.toml"), "[build]\nincremental = true\n").unwrap();
    fs::write(cargo.join("config"), "[build]\nincremental = false\n").unwrap();

    let report = doctor::report(workspace.path()).unwrap();

    assert!(report.mold.repository_default_linker);
    let release = report
        .incremental
        .disabled_profiles
        .iter()
        .find(|profile| profile.profile == "release")
        .unwrap();
    assert_eq!(release.incremental_source.source, ".cargo/config");
}

#[test]
fn build_incremental_overrides_a_profile_setting() {
    let workspace = tempdir().unwrap();
    fs::create_dir_all(workspace.path().join(".cargo")).unwrap();
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[profile.release]\nincremental = true\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join(".cargo/config.toml"),
        "[build]\nincremental = false\n",
    )
    .unwrap();

    let report = doctor::report(workspace.path()).unwrap();
    let release = report
        .incremental
        .disabled_profiles
        .iter()
        .find(|profile| profile.profile == "release")
        .unwrap();

    assert_eq!(release.incremental_source.key, "build.incremental");
    assert_eq!(release.incremental_source.source, ".cargo/config.toml");
}

#[test]
fn ancestor_and_included_cargo_config_change_lane_identity() {
    let root = tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir_all(root.path().join(".cargo")).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join(".cargo/config.toml"),
        "include = [\"incremental.toml\"]\n",
    )
    .unwrap();
    let include = root.path().join(".cargo/incremental.toml");
    fs::write(&include, "[build]\nincremental = true\n").unwrap();
    let workspace = workspace.to_string_lossy().into_owned();
    let enabled = cache::lane_id(&workspace, "stable");

    fs::write(&include, "[build]\nincremental = false\n").unwrap();

    assert_ne!(enabled, cache::lane_id(&workspace, "stable"));
}

#[test]
fn incremental_policy_changes_the_lane_identity() {
    let dir = tempdir().unwrap();
    let manifest = dir.path().join("Cargo.toml");
    fs::write(
        &manifest,
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let workspace = dir.path().to_string_lossy().into_owned();
    let default = cache::lane_id(&workspace, "stable");

    fs::write(
        &manifest,
        "[package]\nname = \"doctor_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[profile.release]\nincremental = true\n",
    )
    .unwrap();
    assert_ne!(default, cache::lane_id(&workspace, "stable"));
}
