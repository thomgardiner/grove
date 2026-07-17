//! Work-coordination tests: a claim is granted, an overlapping claim from another agent
//! is rejected (first-wins), non-overlapping claims coexist, and release/status behave.

use grove::claim::{self, ClaimOutcome, ClaimRequest};
use tempfile::tempdir;

fn req<'a>(root: &'a std::path::Path, agent: &str, scope: &[&str]) -> ClaimRequest<'a> {
    ClaimRequest {
        root,
        repo: root.to_str().unwrap(),
        workspace: Some(root),
        agent: agent.into(),
        task: String::new(),
        scope: scope.iter().map(|s| s.to_string()).collect(),
        branch: None,
        force: false,
    }
}

fn workspace(path: &std::path::Path) {
    std::fs::create_dir_all(path.join("crates/auth/src")).unwrap();
    std::fs::write(
        path.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/auth\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        path.join("crates/auth/Cargo.toml"),
        "[package]\nname = \"auth\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(path.join("crates/auth/src/lib.rs"), "").unwrap();
}

fn workspace_with_root_package(path: &std::path::Path) {
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::create_dir_all(path.join("crates/auth/src")).unwrap();
    std::fs::write(
        path.join("Cargo.toml"),
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\nreadme = \"README.md\"\n\
         [workspace]\nmembers = [\"crates/auth\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(path.join("README.md"), "# root\n").unwrap();
    std::fs::write(path.join("data.txt"), "package asset\n").unwrap();
    std::fs::write(
        path.join("src/lib.rs"),
        "pub const DATA: &[u8] = include_bytes!(\"../data.txt\");\n",
    )
    .unwrap();
    std::fs::write(
        path.join("crates/auth/Cargo.toml"),
        "[package]\nname = \"auth\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(path.join("crates/auth/src/lib.rs"), "").unwrap();
}

fn workspace_with_sole_root_package(path: &std::path::Path) {
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::write(
        path.join("Cargo.toml"),
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(path.join("src/lib.rs"), "").unwrap();
}

#[test]
fn overlapping_claim_from_another_agent_is_rejected() {
    let base = tempdir().unwrap();
    let root = base.path();

    // alice claims crates/auth; granted.
    assert!(matches!(
        claim::claim(&req(root, "alice", &["crates/auth"])).unwrap(),
        ClaimOutcome::Granted { .. }
    ));

    // bob claims a subdirectory of it; overlap => conflict, naming alice.
    match claim::claim(&req(root, "bob", &["crates/auth/src"])).unwrap() {
        ClaimOutcome::Conflict { conflicts, .. } => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].agent, "alice");
        }
        ClaimOutcome::Granted { .. } => panic!("overlap should have been rejected"),
    }

    // bob claims a disjoint area; granted, and both now show on the board.
    assert!(matches!(
        claim::claim(&req(root, "bob", &["crates/checkout"])).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
    assert_eq!(
        claim::status(root, root.to_str().unwrap(), root)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn crate_and_path_specs_share_one_resolved_namespace() {
    let base = tempdir().unwrap();
    let root = base.path();
    let repo = root.join("repo");
    workspace(&repo);
    let mut crate_claim = req(root, "alice", &["crate:auth"]);
    crate_claim.workspace = Some(&repo);
    crate_claim.repo = repo.to_str().unwrap();
    assert!(matches!(
        claim::claim(&crate_claim).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
    let mut path_claim = req(root, "bob", &["crates/auth/src"]);
    path_claim.workspace = Some(&repo);
    path_claim.repo = repo.to_str().unwrap();
    assert!(matches!(
        claim::claim(&path_claim).unwrap(),
        ClaimOutcome::Conflict { .. }
    ));
}

#[test]
fn mixed_workspace_root_package_requires_an_explicit_scope() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    workspace_with_root_package(&repo);

    assert_eq!(
        claim::resolve_scopes(&repo, &["crate:auth".into()]).unwrap(),
        ["crates/auth"]
    );
    let error = claim::resolve_scopes(&repo, &["crate:root".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("root"), "{error}");
    assert!(error.contains("explicit"), "{error}");
    assert!(error.contains("`.`"), "{error}");
}

#[test]
fn sole_root_package_maps_to_the_whole_workspace() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    workspace_with_sole_root_package(&repo);

    assert_eq!(
        claim::resolve_scopes(&repo, &["crate:root".into()]).unwrap(),
        ["."]
    );
}

#[test]
fn refused_root_package_claim_does_not_block_child_workspace_members() {
    let base = tempdir().unwrap();
    let root = base.path();
    let repo = root.join("repo");
    workspace_with_root_package(&repo);

    let mut root_claim = req(root, "alice", &["crate:root"]);
    root_claim.workspace = Some(&repo);
    root_claim.repo = repo.to_str().unwrap();
    assert!(claim::claim(&root_claim).is_err());
    assert!(
        claim::status(root, root.to_str().unwrap(), root)
            .unwrap()
            .is_empty()
    );

    let mut child_claim = req(root, "bob", &["crate:auth"]);
    child_claim.workspace = Some(&repo);
    child_claim.repo = repo.to_str().unwrap();
    assert!(matches!(
        claim::claim(&child_claim).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
}

#[test]
fn same_agent_may_renew_and_release_drops_the_claim() {
    let base = tempdir().unwrap();
    let root = base.path();
    assert!(matches!(
        claim::claim(&req(root, "alice", &["src/login"])).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
    // Same agent re-claiming its own overlapping scope is not a conflict.
    assert!(matches!(
        claim::claim(&req(root, "alice", &["src/login"])).unwrap(),
        ClaimOutcome::Granted { .. }
    ));

    let released = claim::release(root, root.to_str().unwrap(), None, "alice", &[]).unwrap();
    assert!(!released.released.is_empty());
    assert!(
        claim::status(root, root.to_str().unwrap(), root)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn force_overrides_an_overlap() {
    let base = tempdir().unwrap();
    let root = base.path();
    claim::claim(&req(root, "alice", &["crates/auth"])).unwrap();
    let mut forced = req(root, "bob", &["crates/auth"]);
    forced.force = true;
    assert!(matches!(
        claim::claim(&forced).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
    assert_eq!(
        claim::status(root, root.to_str().unwrap(), root)
            .unwrap()
            .len(),
        2
    );
}
