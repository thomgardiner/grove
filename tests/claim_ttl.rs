use grove::claim::{self, ClaimOutcome, ClaimRequest};
use grove::{project, status, task};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

struct Cwd(PathBuf);

impl Drop for Cwd {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore current directory");
    }
}

struct Env(Option<OsString>);

impl Env {
    fn remove() -> Self {
        let value = std::env::var_os("GROVE_CLAIM_TTL_SECS");
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::remove_var("GROVE_CLAIM_TTL_SECS") };
        Self(value)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        match self.0.take() {
            // SAFETY: nextest runs each test in its own process.
            Some(value) => unsafe { std::env::set_var("GROVE_CLAIM_TTL_SECS", value) },
            // SAFETY: nextest runs each test in its own process.
            None => unsafe { std::env::remove_var("GROVE_CLAIM_TTL_SECS") },
        }
    }
}

struct Fixture {
    _base: tempfile::TempDir,
    root: PathBuf,
    zero: PathBuf,
    long: PathBuf,
    zero_repo: String,
    long_repo: String,
}

impl Fixture {
    fn new() -> Self {
        let base = tempdir().expect("create TTL fixture");
        let root = base.path().join("cache");
        let zero = base.path().join("zero");
        let long = base.path().join("long");
        repository(&zero, 0);
        repository(&long, 3600);
        let zero_repo = project::repo_identity(&zero);
        let long_repo = project::repo_identity(&long);
        Self {
            _base: base,
            root,
            zero,
            long,
            zero_repo,
            long_repo,
        }
    }

    fn publish(&self) {
        std::env::set_current_dir(&self.long).expect("enter long-TTL workspace");
        grant(&self.root, &self.zero_repo, &self.zero, "standalone-zero");
        grant(&self.root, &self.long_repo, &self.long, "standalone-long");
        begin(&self.root, &self.zero, "task-zero");
        begin(&self.root, &self.long, "task-long");
    }

    fn assert_order(&self, current: &Path, long_first: bool) {
        std::env::set_current_dir(current).expect("change evaluation current directory");
        let (zero, long) = if long_first {
            let long = claim::status(&self.root, &self.long_repo, &self.long).unwrap();
            let zero = claim::status(&self.root, &self.zero_repo, &self.zero).unwrap();
            (zero, long)
        } else {
            let zero = claim::status(&self.root, &self.zero_repo, &self.zero).unwrap();
            let long = claim::status(&self.root, &self.long_repo, &self.long).unwrap();
            (zero, long)
        };
        assert!(!has_agent(&zero, "standalone-zero"));
        assert!(has_agent(&long, "standalone-long"));
        let states = if long_first {
            (
                state(&self.root, &self.long, "task-long"),
                state(&self.root, &self.zero, "task-zero"),
            )
        } else {
            (
                state(&self.root, &self.zero, "task-zero"),
                state(&self.root, &self.long, "task-long"),
            )
        };
        assert_eq!(
            states,
            if long_first {
                ("idle".into(), "stalled".into())
            } else {
                ("stalled".into(), "idle".into())
            }
        );
    }
}

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    );
}

fn repository(path: &Path, ttl: u64) {
    std::fs::create_dir_all(path.join("src")).unwrap();
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "claim@example.invalid"]);
    git(path, &["config", "user.name", "claim-test"]);
    std::fs::write(
        path.join("Cargo.toml"),
        "[package]\nname='claim_fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(path.join("src/lib.rs"), "").unwrap();
    std::fs::write(
        path.join(".grove.toml"),
        format!("claim_ttl_secs = {ttl}\n"),
    )
    .unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "-m", "init"]);
}

fn grant(root: &Path, repo: &str, workspace: &Path, agent: &str) {
    let request = ClaimRequest {
        root,
        repo,
        workspace: Some(workspace),
        agent: agent.into(),
        task: "ttl".into(),
        scope: vec!["docs".into()],
        branch: None,
        force: false,
    };
    assert!(matches!(
        claim::claim(&request).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
}

fn begin(root: &Path, workspace: &Path, agent: &str) {
    task::begin(task::Begin {
        root,
        workspace,
        agent: agent.into(),
        description: "ttl".into(),
        scope: vec!["src".into()],
    })
    .unwrap();
}

fn has_agent(claims: &[claim::Claim], agent: &str) -> bool {
    claims.iter().any(|claim| claim.agent == agent)
}

fn state(root: &Path, workspace: &Path, agent: &str) -> String {
    let report = serde_json::to_value(status::report(root, workspace).unwrap()).unwrap();
    report["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["agent"] == agent)
        .unwrap()["status"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn claims_and_task_status_use_each_repository_ttl_in_either_order() {
    let _env = Env::remove();
    let _cwd = Cwd(std::env::current_dir().expect("read current directory"));
    let fixture = Fixture::new();
    fixture.publish();
    std::thread::sleep(Duration::from_millis(1100));
    fixture.assert_order(&fixture.long, false);
    fixture.assert_order(&fixture.zero, true);
}

#[test]
fn standalone_claim_requires_workspace_unless_ttl_is_overridden() {
    let _env = Env::remove();
    let base = tempdir().unwrap();
    let request = ClaimRequest {
        root: base.path(),
        repo: "/repo/.git",
        workspace: None,
        agent: "alice".into(),
        task: String::new(),
        scope: vec!["src".into()],
        branch: None,
        force: false,
    };
    let error = claim::claim(&request)
        .err()
        .expect("workspace-less configured claim fails");
    assert!(error.to_string().contains("requires a workspace"));
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("GROVE_CLAIM_TTL_SECS", "0") };
    assert!(matches!(
        claim::claim(&request).unwrap(),
        ClaimOutcome::Granted { .. }
    ));
}

#[test]
fn mismatched_workspace_cannot_expire_another_repository_claim() {
    let _env = Env::remove();
    let fixture = Fixture::new();
    grant(
        &fixture.root,
        &fixture.long_repo,
        &fixture.long,
        "standalone-long",
    );
    std::thread::sleep(Duration::from_millis(1100));

    let error = claim::status(&fixture.root, &fixture.long_repo, &fixture.zero)
        .err()
        .expect("mismatched status workspace fails");
    assert!(error.to_string().contains("belongs to repository"));
    let claims = claim::status(&fixture.root, &fixture.long_repo, &fixture.long).unwrap();
    assert!(has_agent(&claims, "standalone-long"));

    let request = ClaimRequest {
        root: &fixture.root,
        repo: &fixture.long_repo,
        workspace: Some(&fixture.zero),
        agent: "mismatch".into(),
        task: "ttl".into(),
        scope: vec!["other".into()],
        branch: None,
        force: false,
    };
    assert!(claim::claim(&request).is_err());
}
