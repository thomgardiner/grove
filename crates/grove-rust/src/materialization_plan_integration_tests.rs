use super::*;

#[test]
fn external_target_falls_back_to_full() {
    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    write(
        &repo,
        "Cargo.toml",
        "[workspace]\nmembers=['crates/a']\nresolver='2'\n",
    );
    write(
        &repo,
        "crates/a/Cargo.toml",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n\
         [lib]\npath='../../../outside.rs'\n",
    );
    write(root.path(), "outside.rs", "pub fn outside() {}\n");
    let base = commit(&repo);
    let plan = plan_scope(&repo, &base, "crate:a");
    assert_eq!(plan.fallback_reason, Some(FallbackReason::RootScope));
}

#[test]
fn external_path_dependency_falls_back_to_full() {
    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    write(
        &repo,
        "Cargo.toml",
        "[workspace]\nmembers=['crates/a']\nresolver='2'\n",
    );
    package(
        &repo,
        "a",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n\
         [dependencies]\nexternal={path='../../../external'}\n",
    );
    write(
        root.path(),
        "external/Cargo.toml",
        "[package]\nname='external'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(root.path(), "external/src/lib.rs", "pub fn external() {}\n");
    let base = commit(&repo);
    let plan = plan_scope(&repo, &base, "crate:a");
    assert_eq!(plan.fallback_reason, Some(FallbackReason::RootScope));
}

#[test]
fn missing_target_falls_back_to_full() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['crates/a']\nresolver='2'\n",
    );
    write(
        repo.path(),
        "crates/a/Cargo.toml",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n\
         [lib]\npath='missing.rs'\n",
    );
    let base = commit(repo.path());
    let plan = plan_scope(repo.path(), &base, "crate:a");
    assert_eq!(plan.fallback_reason, Some(FallbackReason::RootScope));
}

#[test]
fn ignored_workspace_member_is_not_treated_as_base_input() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), ".gitignore", "crates/generated/\n");
    write(
        repo.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['crates/*']\nresolver='2'\n",
    );
    package(
        repo.path(),
        "a",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n",
    );
    let base = commit(repo.path());
    package(
        repo.path(),
        "generated",
        "[package]\nname='generated'\nversion='0.1.0'\nedition='2024'\n",
    );
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
    assert!(error.contains("ignored Cargo"), "{error}");
    assert!(
        git::capture(repo.path(), &["status", "--porcelain"])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn absent_ignored_lock_is_rejected_before_metadata() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), ".gitignore", "Cargo.lock\n");
    write(
        repo.path(),
        "Cargo.toml",
        "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "src/lib.rs", "pub fn root() {}\n");
    let base = commit(repo.path());
    fs::remove_file(repo.path().join("Cargo.lock")).unwrap();
    let scopes = vec!["src".into()];
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
    assert!(error.contains("prospective Cargo lockfile"), "{error}");
    assert!(!repo.path().join("Cargo.lock").exists());
}

#[test]
fn ignored_repository_config_is_rejected_before_metadata() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), ".gitignore", ".grove.toml\n");
    write(
        repo.path(),
        "Cargo.toml",
        "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "src/lib.rs", "pub fn root() {}\n");
    let base = commit(repo.path());
    let config = repo.path().join(".grove.toml");
    write(
        repo.path(),
        ".grove.toml",
        "[worktree]\nmaterialize=['schemas']\n",
    );
    let scopes = vec!["src".into()];
    let extras = vec!["schemas".into()];
    let error = plan(PlanInput {
        workspace: repo.path(),
        base_oid: &base,
        scopes: &scopes,
        extras: &extras,
        config: Some(&config),
        fingerprint: &fingerprint(),
        planned_at: 1,
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("ignored Cargo or Grove"), "{error}");
}

#[test]
fn ignored_included_cargo_config_is_rejected_before_metadata() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), ".gitignore", "config/local.toml\n");
    write(
        repo.path(),
        "Cargo.toml",
        "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "src/lib.rs", "pub fn root() {}\n");
    write(
        repo.path(),
        ".cargo/config.toml",
        "include=['../config/local.toml']\n",
    );
    write(repo.path(), "config/local.toml", "[net]\noffline=true\n");
    let base = commit(repo.path());
    let scopes = vec!["src".into()];
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
    assert!(error.contains("selected base"), "{error}");
    assert!(
        git::capture(repo.path(), &["status", "--porcelain"])
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn ignored_symlinked_cargo_config_is_rejected_before_metadata() {
    use std::os::unix::fs::symlink;

    let repo = TempDir::new().unwrap();
    write(repo.path(), ".gitignore", "ignored/local.toml\n");
    write(
        repo.path(),
        "Cargo.toml",
        "[package]\nname='root'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "src/lib.rs", "pub fn root() {}\n");
    write(
        repo.path(),
        ".cargo/config.toml",
        "include=['../ignored/local.toml']\n",
    );
    write(repo.path(), "config/shared.toml", "[net]\noffline=true\n");
    fs::create_dir_all(repo.path().join("ignored")).unwrap();
    symlink(
        repo.path().join("config/shared.toml"),
        repo.path().join("ignored/local.toml"),
    )
    .unwrap();
    let base = commit(repo.path());
    let scopes = vec!["src".into()];
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
    assert!(error.contains("must not be a symlink"), "{error}");
    assert!(
        git::capture(repo.path(), &["status", "--porcelain"])
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn tracked_symlinked_member_manifest_is_rejected_before_metadata() {
    use std::os::unix::fs::symlink;

    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['crates/a']\nresolver='2'\n",
    );
    write(
        repo.path(),
        "manifests/a.toml",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "crates/a/src/lib.rs", "pub fn a() {}\n");
    symlink(
        repo.path().join("manifests/a.toml"),
        repo.path().join("crates/a/Cargo.toml"),
    )
    .unwrap();
    let base = commit(repo.path());
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
    assert!(
        error.contains("Cargo manifest must not be a symlink"),
        "{error}"
    );
}

#[test]
fn nested_workspace_uses_git_repository_coordinates() {
    let repo = TempDir::new().unwrap();
    let workspace = repo.path().join("workspace");
    write(
        &workspace,
        "Cargo.toml",
        "[workspace]\nmembers=['crates/a']\nresolver='2'\n",
    );
    package(
        &workspace,
        "a",
        "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n\
         [dependencies]\nshared={path='../../../shared'}\n",
    );
    write(
        repo.path(),
        "shared/Cargo.toml",
        "[package]\nname='shared'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(repo.path(), "shared/src/lib.rs", "pub fn shared() {}\n");
    write(
        &workspace,
        ".cargo/config.toml",
        "include=['../../config/shared.toml']\n[build]\nincremental=true\n",
    );
    write(repo.path(), "config/shared.toml", "[net]\noffline=true\n");
    write(repo.path(), "assets/large.bin", &"x".repeat(4096));
    lock(&workspace);
    let base = commit(repo.path());
    let plan = plan_scope(&workspace, &base, "crate:a");
    assert_eq!(plan.closure_packages, ["a", "shared"]);
    assert_eq!(plan.closure_cones, ["shared", "workspace/crates/a"]);
    assert_eq!(plan.support_cones, ["config", "workspace/.cargo"]);
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
}

#[test]
fn hidden_index_edits_are_rejected() {
    let (repo, base) = workspace();
    git::run(
        repo.path(),
        &["update-index", "--skip-worktree", "Cargo.toml"],
    )
    .unwrap();
    write(
        repo.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['crates/a']\nresolver='2'\n",
    );
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
    assert!(error.contains("index flags"), "{error}");
}
