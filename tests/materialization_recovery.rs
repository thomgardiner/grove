#![allow(clippy::unwrap_used)]

use grove::config::Config;
use grove::worktree::{self, AcquireRequest, Lease, MaterializationMode, MaterializationRecord};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{TempDir, tempdir};

struct Repo {
    _dir: TempDir,
    root: PathBuf,
    repo: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        write(
            &repo,
            "Cargo.toml",
            "[workspace]\nmembers=['crates/*']\nresolver='2'\n",
        );
        package(&repo, "app");
        package(&repo, "tool");
        package(&repo, "large");
        write(
            &repo,
            "crates/large/assets/payload.bin",
            &"payload".repeat(1024),
        );
        run(&repo, "cargo", &["generate-lockfile"]);
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "core.autocrlf", "false"]);
        git(&repo, &["config", "user.email", "expand@example.invalid"]);
        git(&repo, &["config", "user.name", "Expand Test"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        Self {
            root: dir.path().join("cache"),
            _dir: dir,
            repo,
        }
    }

    fn acquire(&self, agent: &str) -> PathBuf {
        worktree::scoped(
            &AcquireRequest {
                root: &self.root,
                cwd: &self.repo,
                agent: agent.into(),
                branch: None,
                base: "HEAD".into(),
            },
            &["crate:app".into()],
            &Config::resolve(&self.repo),
        )
        .unwrap()
    }

    fn lease(&self, workspace: &Path) -> Lease {
        let path = self.root.join("leases").join(format!(
            "{}.json",
            grove::cache::lane_id(
                &workspace.to_string_lossy(),
                &grove::project::toolchain(workspace)
            )
        ));
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }
}

#[test]
fn expansion_is_monotonic_and_keeps_unrelated_payload_absent() {
    let repo = Repo::new();
    let workspace = repo.acquire("expand");
    write(&workspace, "notes/untracked.txt", "keep me");
    write(
        &workspace,
        "crates/app/src/lib.rs",
        &"pub fn changed() {}\n".repeat(1024),
    );

    let record = worktree::expand(&repo.root, &workspace, &["crate:tool".into()])
        .unwrap()
        .unwrap();

    assert_eq!(record_mode(&record), MaterializationMode::Sparse);
    assert!(workspace.join("crates/tool/src/lib.rs").is_file());
    assert!(!workspace.join("crates/large/assets/payload.bin").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("notes/untracked.txt")).unwrap(),
        "keep me"
    );
    assert!(
        fs::read_to_string(workspace.join("crates/app/src/lib.rs"))
            .unwrap()
            .contains("changed")
    );
    let lease = repo.lease(&workspace);
    assert_eq!(
        materialization(&lease)["expansion_count"],
        Value::from(1_u64)
    );
    worktree::full(&repo.root, &workspace).unwrap();
    worktree::release(&repo.root, &workspace).unwrap();
}

#[test]
fn full_conversion_preserves_untracked_files_and_populates_payload() {
    let repo = Repo::new();
    let workspace = repo.acquire("full");
    write(&workspace, "notes/untracked.txt", "keep me");
    fs::remove_file(workspace.join("crates/app/src/lib.rs")).unwrap();

    let record = worktree::full(&repo.root, &workspace).unwrap().unwrap();

    assert_eq!(record_mode(&record), MaterializationMode::Full);
    assert!(workspace.join("crates/large/assets/payload.bin").is_file());
    assert_eq!(
        fs::read_to_string(workspace.join("notes/untracked.txt")).unwrap(),
        "keep me"
    );
    assert!(
        !workspace.join("crates/app/src/lib.rs").exists(),
        "full conversion must preserve a real tracked deletion"
    );
    worktree::release(&repo.root, &workspace).unwrap();
}

#[test]
fn interrupted_full_intent_recovers_before_or_after_git_mutation() {
    for git_changed in [false, true] {
        let repo = Repo::new();
        let workspace = repo.acquire(if git_changed {
            "after-git"
        } else {
            "before-git"
        });
        let lease = repo.lease(&workspace);
        let intent = intent_path(&repo.root, &lease);
        fs::create_dir_all(intent.parent().unwrap()).unwrap();
        fs::write(&intent, full_intent(&lease)).unwrap();
        if git_changed {
            git(&workspace, &["sparse-checkout", "disable"]);
        }

        let record = worktree::full(&repo.root, &workspace).unwrap().unwrap();

        assert_eq!(record_mode(&record), MaterializationMode::Full);
        assert!(!intent.exists());
        assert!(workspace.join("crates/large/assets/payload.bin").is_file());
        worktree::release(&repo.root, &workspace).unwrap();
    }
}

fn full_intent(lease: &Lease) -> Vec<u8> {
    let prior = materialization(lease);
    serde_json::to_vec_pretty(&json!({
        "schema_version": 1,
        "repo": lease.repo,
        "workspace": lease.workspace,
        "cargo_dir": ".",
        "branch": lease.branch,
        "base_oid": lease.base_oid,
        "prior": prior,
        "desired_mode": "full",
        "requested_scopes": prior["requested_scopes"],
        "closure_cones": prior["closure_cones"],
        "support_cones": prior["support_cones"],
        "current_cones": prior["current_cones"],
        "desired_cones": [],
        "plan": null,
        "created_at": 1
    }))
    .unwrap()
}

fn intent_path(root: &Path, lease: &Lease) -> PathBuf {
    root.join("materializations")
        .join(grove::cache::repo_slug(&lease.repo))
        .join(format!(
            "{}.json",
            grove::cache::lane_id(&lease.workspace, &lease.toolchain)
        ))
}

fn materialization(lease: &Lease) -> Value {
    serde_json::to_value(lease).unwrap()["materialization"].clone()
}

fn record_mode(record: &MaterializationRecord) -> MaterializationMode {
    serde_json::from_value(serde_json::to_value(record).unwrap()["mode"].clone()).unwrap()
}

fn package(repo: &Path, name: &str) {
    write(
        repo,
        &format!("crates/{name}/Cargo.toml"),
        &format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n"),
    );
    write(
        repo,
        &format!("crates/{name}/src/lib.rs"),
        "pub fn marker() {}\n",
    );
}

fn write(repo: &Path, path: &str, contents: &str) {
    let path = repo.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    run(dir, "git", args);
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
