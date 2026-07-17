use super::*;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

struct Fixture {
    _base: TempDir,
    repo: PathBuf,
    root: PathBuf,
    base: String,
}

impl Fixture {
    fn new() -> Self {
        let base = TempDir::new().unwrap();
        let repo = base.path().join("repo");
        write(
            &repo,
            "Cargo.toml",
            "[workspace]\nmembers=['crates/*']\nresolver='2'\n",
        );
        package(&repo, "app");
        package(&repo, "large");
        write(
            &repo,
            "crates/large/assets/payload.bin",
            &"payload".repeat(1024),
        );
        run(&repo, "cargo", &["generate-lockfile"]);
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &["config", "user.email", "materialized@example.invalid"],
        );
        git(&repo, &["config", "user.name", "Materialized Test"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let oid = git_out(&repo, &["rev-parse", "HEAD"]);
        Self {
            root: base.path().join("cache"),
            _base: base,
            repo,
            base: oid,
        }
    }

    fn request(&self, base: &str, agent: &str) -> AcquireRequest<'_> {
        AcquireRequest {
            root: &self.root,
            cwd: &self.repo,
            agent: agent.into(),
            branch: None,
            base: base.into(),
        }
    }

    fn lease(&self, workspace: &Path) -> Lease {
        let workspace = cache::canonical_path(workspace)
            .to_string_lossy()
            .into_owned();
        find_lease(&self.root, &workspace).unwrap().unwrap().1
    }
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

fn git(dir: &Path, args: &[&str]) {
    run(dir, "git", args);
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn scoped_acquire_omits_unrelated_payload_and_publishes_proof() {
    let fixture = Fixture::new();
    let config = config::Config::resolve(&fixture.repo);
    let workspace = scoped(
        &fixture.request("HEAD", "sparse"),
        &["crate:app".into()],
        &config,
    )
    .unwrap();

    assert!(workspace.join("crates/app/src/lib.rs").is_file());
    assert!(workspace.join("crates/large/src/lib.rs").is_file());
    assert!(!workspace.join("crates/large/assets/payload.bin").exists());
    let record = fixture.lease(&workspace).materialization.unwrap();
    assert_eq!(record.mode, MaterializationMode::Sparse);
    assert_eq!(
        record.source_cargo_fingerprint,
        record.candidate_cargo_fingerprint
    );
    assert!(record.selected_git_blob_bytes < record.full_git_blob_bytes);
    crate::worktree::release(&fixture.root, &workspace).unwrap();
}

#[test]
fn scoped_acquire_never_checks_out_an_unselected_payload() {
    let fixture = Fixture::new();
    let trace = fixture._base.path().join("git-trace.json");
    // SAFETY: nextest runs each test in its own process.
    unsafe {
        std::env::set_var("GIT_TRACE2_EVENT", &trace);
    }
    let config = config::Config::resolve(&fixture.repo);

    let workspace = scoped(
        &fixture.request("HEAD", "no-full-checkout"),
        &["crate:app".into()],
        &config,
    )
    .unwrap();
    // SAFETY: nextest runs each test in its own process.
    unsafe {
        std::env::remove_var("GIT_TRACE2_EVENT");
    }

    assert!(fs::read_to_string(trace).unwrap().contains("--no-checkout"));
    assert!(workspace.join("crates/app/src/lib.rs").is_file());
    assert!(!workspace.join("crates/large/assets/payload.bin").exists());
    crate::worktree::release(&fixture.root, &workspace).unwrap();
}

#[test]
fn root_scope_falls_back_to_a_proved_full_checkout() {
    let fixture = Fixture::new();
    let config = config::Config::resolve(&fixture.repo);

    let workspace = scoped(
        &fixture.request("HEAD", "root-scope"),
        &["Cargo.toml".into()],
        &config,
    )
    .unwrap();

    let record = fixture.lease(&workspace).materialization.unwrap();
    assert_eq!(record.mode, MaterializationMode::Full);
    assert_eq!(record.fallback_reason, Some(FallbackReason::RootScope));
    assert_eq!(
        record.source_cargo_fingerprint,
        record.candidate_cargo_fingerprint
    );
    assert!(workspace.join("crates/large/assets/payload.bin").is_file());
    crate::worktree::release(&fixture.root, &workspace).unwrap();
}

#[test]
fn scoped_acquire_bootstraps_the_selected_base_despite_dirty_newer_source() {
    let fixture = Fixture::new();
    write(
        &fixture.repo,
        "crates/app/src/lib.rs",
        "pub fn newer() {}\n",
    );
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "newer"]);
    write(&fixture.repo, "untracked.txt", "source dirt");
    let config = config::Config::resolve(&fixture.repo);

    let workspace = scoped(
        &fixture.request(&fixture.base, "old-base"),
        &["crate:app".into()],
        &config,
    )
    .unwrap();

    assert_eq!(git_out(&workspace, &["rev-parse", "HEAD"]), fixture.base);
    assert!(
        fs::read_to_string(workspace.join("crates/app/src/lib.rs"))
            .unwrap()
            .contains("marker")
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("untracked.txt")).unwrap(),
        "source dirt"
    );
    crate::worktree::release(&fixture.root, &workspace).unwrap();
}

#[test]
fn interrupted_full_bootstrap_recovers_as_explicit_full_materialization() {
    let fixture = Fixture::new();
    let config = config::Config::resolve(&fixture.repo);
    let request = fixture.request("HEAD", "recovery");
    let ctx = repo_context(&fixture.repo).unwrap();
    let root_dir = worktree_root(&config, &fixture.root, &ctx.repo_id, &ctx.main_root);
    fs::create_dir_all(&root_dir).unwrap();
    let slot = slot(
        &request,
        &ctx,
        &cache::canonical_path(&root_dir),
        "grove/recovery",
    )
    .unwrap();
    let mut acquisition = intent(
        &request,
        &ctx,
        &slot,
        Some(state(&request, &config, &["crate:app".into()]).unwrap()),
    );
    let file = write_intent(&fixture.root, &acquisition).unwrap();
    add(&Add {
        main: &ctx.main_root,
        branch: &acquisition.branch,
        existing: slot.existing,
        workspace: &slot.workspace,
        base: &acquisition.base_oid,
        checkout: true,
    })
    .unwrap();
    acquisition.materialization.as_mut().unwrap().plan = None;
    assert!(file.exists());

    let recovered = reconcile(&fixture.root, &ctx).unwrap();

    assert_eq!(recovered.len(), 1);
    assert!(!file.exists());
    let record = recovered[0].materialization.as_ref().unwrap();
    assert_eq!(record.mode, MaterializationMode::Full);
    assert_eq!(record.fallback_reason, Some(FallbackReason::RecoveryFull));
    assert!(
        slot.workspace
            .join("crates/large/assets/payload.bin")
            .is_file()
    );
    crate::worktree::release(&fixture.root, &slot.workspace).unwrap();
}
