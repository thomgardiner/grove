//! Work-coordination tests: a claim is granted, an overlapping claim from another agent
//! is rejected (first-wins), non-overlapping claims coexist, and release/status behave.

use grove::claim::{self, ClaimOutcome, ClaimRequest};
use tempfile::tempdir;

fn req<'a>(root: &'a std::path::Path, agent: &str, scope: &[&str]) -> ClaimRequest<'a> {
    ClaimRequest {
        root,
        repo: "/repo/.git",
        agent: agent.into(),
        task: String::new(),
        scope: scope.iter().map(|s| s.to_string()).collect(),
        branch: None,
        force: false,
    }
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
    assert_eq!(claim::status(root, "/repo/.git").len(), 2);
}

#[test]
fn crate_and_path_specs_are_distinct_namespaces() {
    let base = tempdir().unwrap();
    let root = base.path();
    assert!(matches!(
        claim::claim(&req(root, "alice", &["crate:auth"])).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
    // A path that is not the crate spec does not overlap it.
    assert!(matches!(
        claim::claim(&req(root, "bob", &["crates/auth"])).unwrap(),
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

    let released = claim::release(root, "/repo/.git", "alice", &[]).unwrap();
    assert!(!released.released.is_empty());
    assert!(claim::status(root, "/repo/.git").is_empty());
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
    assert_eq!(claim::status(root, "/repo/.git").len(), 2);
}
