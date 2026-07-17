use grove::worktree::{self, AcquireRequest};
use std::fs::{self, FileTimes, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};
use tempfile::{TempDir, tempdir};

#[path = "worktree_salvage/recovery.rs"]
mod recovery;

struct Fixture {
    _base: TempDir,
    repo: PathBuf,
    cache: PathBuf,
    worktree: PathBuf,
    branch: String,
}

impl Fixture {
    fn new() -> Self {
        let base = tempdir().unwrap();
        let repo = base.path().join("repo");
        let cache = base.path().join("cache");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "salvage@example.invalid"]);
        git(&repo, &["config", "user.name", "salvage-test"]);
        fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let branch = "grove/salvage-test".to_string();
        let worktree = worktree::acquire(&AcquireRequest {
            root: &cache,
            cwd: &repo,
            agent: "salvage-test".into(),
            branch: Some(branch.clone()),
            base: "HEAD".into(),
        })
        .unwrap();
        Self {
            _base: base,
            repo,
            cache,
            worktree,
            branch,
        }
    }

    fn release(&self) -> anyhow::Result<worktree::ReleaseOutcome> {
        worktree::release(&self.cache, &self.worktree)
    }

    fn release_error(&self) -> anyhow::Error {
        match self.release() {
            Ok(_) => panic!("release unexpectedly succeeded"),
            Err(error) => error,
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn git_text(dir: &Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn show(repo: &Path, reference: &str, path: &str) -> Vec<u8> {
    git_bytes(repo, &["show", &format!("{reference}:{path}")])
}

fn index_path(worktree: &Path) -> PathBuf {
    let path = PathBuf::from(git_text(worktree, &["rev-parse", "--git-path", "index"]));
    if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    }
}

fn salvage_refs(repo: &Path) -> String {
    git_text(
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/grove/salvage"],
    )
}

fn assert_archived_index(fixture: &Fixture, reference: &str) {
    let archived = fixture.repo.join("archived.index");
    fs::write(
        &archived,
        show(&fixture.repo, reference, ".grove-salvage/index"),
    )
    .unwrap();
    let staged = Command::new("git")
        .args(["ls-files", "--stage", "tracked.txt"])
        .env("GIT_INDEX_FILE", &archived)
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(staged.status.success());
    let staged = String::from_utf8(staged.stdout).unwrap();
    let object = staged.split_whitespace().nth(1).unwrap();
    assert_eq!(
        git_bytes(&fixture.repo, &["cat-file", "blob", object]),
        b"staged\n"
    );
}

#[cfg(unix)]
fn assert_archived_symlink(fixture: &Fixture, reference: &str) {
    let entry = git_text(&fixture.repo, &["ls-tree", reference, "link ü"]);
    assert!(entry.starts_with("120000 blob "));
    assert_eq!(show(&fixture.repo, reference, "link ü"), b"tracked.txt");
}

#[test]
fn release_preserves_staged_unstaged_untracked_and_symlink_state() {
    let fixture = Fixture::new();
    let original_head = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    fs::write(fixture.worktree.join("tracked.txt"), "staged\n").unwrap();
    git(&fixture.worktree, &["add", "tracked.txt"]);
    fs::write(fixture.worktree.join("tracked.txt"), "unstaged\n").unwrap();
    let unicode = "notes ü with spaces.txt";
    fs::write(fixture.worktree.join(unicode), "untracked\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("tracked.txt", fixture.worktree.join("link ü")).unwrap();
    let original_index = fs::read(index_path(&fixture.worktree)).unwrap();

    let outcome = fixture.release().unwrap();
    let reference = outcome.saved_to.expect("dirty state has a salvage ref");

    assert!(reference.starts_with("refs/grove/salvage/"));
    assert!(!fixture.worktree.exists());
    assert_eq!(
        show(&fixture.repo, &reference, "tracked.txt"),
        b"unstaged\n"
    );
    assert_eq!(show(&fixture.repo, &reference, unicode), b"untracked\n");
    assert_eq!(
        show(&fixture.repo, &reference, ".grove-salvage/head"),
        format!("{original_head}\n").as_bytes()
    );
    assert_eq!(
        show(&fixture.repo, &reference, ".grove-salvage/index"),
        original_index
    );
    assert_eq!(
        show(&fixture.repo, &fixture.branch, "tracked.txt"),
        b"unstaged\n"
    );
    assert_archived_index(&fixture, &reference);
    #[cfg(unix)]
    assert_archived_symlink(&fixture, &reference);
}

#[test]
fn unresolved_conflict_is_refused_without_index_or_ref_mutation() {
    let fixture = Fixture::new();
    let main = git_text(&fixture.repo, &["symbolic-ref", "--short", "HEAD"]);
    fs::write(fixture.repo.join("tracked.txt"), "main\n").unwrap();
    git(&fixture.repo, &["commit", "-qam", "main change"]);
    fs::write(fixture.worktree.join("tracked.txt"), "agent\n").unwrap();
    git(&fixture.worktree, &["commit", "-qam", "agent change"]);
    let merge = git_output(&fixture.worktree, &["merge", "--no-edit", &main]);
    assert!(!merge.status.success(), "fixture must create a conflict");
    let before_index = fs::read(index_path(&fixture.worktree)).unwrap();
    let before_status = git_bytes(
        &fixture.worktree,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    );

    let error = fixture.release_error();

    assert!(error.to_string().contains("unresolved index entries"));
    assert_eq!(
        fs::read(index_path(&fixture.worktree)).unwrap(),
        before_index
    );
    assert_eq!(
        git_bytes(
            &fixture.worktree,
            &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        ),
        before_status
    );
    assert!(fixture.worktree.exists());
    assert!(salvage_refs(&fixture.repo).is_empty());
}

#[test]
fn intent_to_add_is_refused_without_index_or_ref_mutation() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join("intent.txt"), "future\n").unwrap();
    git(&fixture.worktree, &["add", "-N", "intent.txt"]);
    let before_index = fs::read(index_path(&fixture.worktree)).unwrap();

    let error = fixture.release_error();

    assert!(error.to_string().contains("intent-to-add"));
    assert_eq!(
        fs::read(index_path(&fixture.worktree)).unwrap(),
        before_index
    );
    assert!(fixture.worktree.join("intent.txt").exists());
    assert!(salvage_refs(&fixture.repo).is_empty());
}

#[test]
fn clean_uninitialized_gitlink_does_not_block_release() {
    let fixture = Fixture::new();
    let commit = git_text(&fixture.worktree, &["rev-parse", "HEAD"]);
    fs::create_dir_all(fixture.worktree.join("deps/submodule")).unwrap();
    git(
        &fixture.worktree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},deps/submodule"),
        ],
    );
    git(&fixture.worktree, &["commit", "-qm", "add gitlink"]);

    let outcome = fixture.release().unwrap();

    assert!(outcome.saved_to.is_none());
    assert!(!fixture.worktree.exists());
}

#[test]
fn populated_gitlink_is_still_refused_without_removing_the_worktree() {
    let fixture = Fixture::new();
    let commit = git_text(&fixture.worktree, &["rev-parse", "HEAD"]);
    fs::create_dir_all(fixture.worktree.join("deps/submodule")).unwrap();
    git(
        &fixture.worktree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},deps/submodule"),
        ],
    );
    git(&fixture.worktree, &["commit", "-qm", "add gitlink"]);
    fs::write(fixture.worktree.join("deps/submodule/nested"), "state\n").unwrap();

    let error = fixture.release_error();

    assert!(error.to_string().contains("submodule state"));
    assert!(fixture.worktree.join("deps/submodule/nested").is_file());
    assert!(salvage_refs(&fixture.repo).is_empty());
}

#[test]
fn ignored_files_are_refused_without_removing_the_worktree() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&fixture.worktree, &["add", ".gitignore"]);
    git(&fixture.worktree, &["commit", "-qm", "ignore fixture"]);
    fs::write(fixture.worktree.join("ignored.txt"), "private\n").unwrap();
    assert!(
        git_bytes(&fixture.worktree, &["status", "--porcelain"]).is_empty(),
        "fixture must be invisible to ordinary status"
    );
    let error = fixture.release_error();
    assert!(error.to_string().contains("ignored path"));
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("ignored.txt")).unwrap(),
        "private\n"
    );
    assert!(salvage_refs(&fixture.repo).is_empty());
}

#[cfg(unix)]
#[test]
fn ignored_files_created_by_clean_filters_are_refused() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(
        fixture.worktree.join(".gitattributes"),
        "tracked.txt filter=createignored\n",
    )
    .unwrap();
    git(&fixture.worktree, &["add", ".gitignore", ".gitattributes"]);
    git(&fixture.worktree, &["commit", "-qm", "filter fixture"]);
    git(
        &fixture.worktree,
        &[
            "config",
            "filter.createignored.clean",
            "sh -c 'printf private > ignored.txt; cat'",
        ],
    );
    let error = fixture.release_error();
    assert!(error.to_string().contains("ignored path"));
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("ignored.txt")).unwrap(),
        "private"
    );
    assert!(salvage_refs(&fixture.repo).is_empty());
}

#[test]
fn dry_run_reap_reports_intent_to_add_as_blocked() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join("intent.txt"), "future\n").unwrap();
    git(&fixture.worktree, &["add", "-N", "intent.txt"]);
    let report = worktree::reap(&fixture.cache, &fixture.repo, 0, true).unwrap();
    assert!(report.reaped.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("intent-to-add"));
    assert!(fixture.worktree.join("intent.txt").is_file());
    assert!(salvage_refs(&fixture.repo).is_empty());
}

#[test]
fn release_hashes_content_even_when_git_status_is_racily_clean() {
    let fixture = Fixture::new();
    git(&fixture.worktree, &["config", "core.trustctime", "false"]);
    git(&fixture.worktree, &["config", "core.checkStat", "minimal"]);
    let old = UNIX_EPOCH + Duration::from_secs(100_000);
    let tracked = fixture.worktree.join("tracked.txt");
    OpenOptions::new()
        .write(true)
        .open(&tracked)
        .unwrap()
        .set_times(FileTimes::new().set_modified(old))
        .unwrap();
    git(&fixture.worktree, &["update-index", "--refresh"]);
    fs::write(&tracked, "next\n").unwrap();
    OpenOptions::new()
        .write(true)
        .open(&tracked)
        .unwrap()
        .set_times(FileTimes::new().set_modified(old))
        .unwrap();
    assert!(
        git_bytes(&fixture.worktree, &["status", "--porcelain"]).is_empty(),
        "fixture must be racily clean"
    );

    let outcome = fixture.release().unwrap();
    let reference = outcome
        .saved_to
        .expect("content hashing detects racily clean work");

    assert_eq!(show(&fixture.repo, &reference, "tracked.txt"), b"next\n");
    assert_eq!(
        show(&fixture.repo, &fixture.branch, "tracked.txt"),
        b"next\n"
    );
}

#[test]
fn sparse_absence_is_not_deleted_and_vivified_work_is_saved() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.worktree.join("keep")).unwrap();
    fs::create_dir_all(fixture.worktree.join("omit")).unwrap();
    fs::write(fixture.worktree.join("keep/one.txt"), "one\n").unwrap();
    fs::write(fixture.worktree.join("omit/two.txt"), "two\n").unwrap();
    fs::write(fixture.worktree.join("omit/three.txt"), "three\n").unwrap();
    git(&fixture.worktree, &["add", "."]);
    git(&fixture.worktree, &["commit", "-qm", "sparse fixture"]);
    git(
        &fixture.worktree,
        &["config", "extensions.worktreeConfig", "true"],
    );
    git(
        &fixture.worktree,
        &[
            "sparse-checkout",
            "set",
            "--cone",
            "--no-sparse-index",
            "keep",
        ],
    );
    assert!(!fixture.worktree.join("omit/three.txt").exists());
    fs::create_dir_all(fixture.worktree.join("omit")).unwrap();
    fs::write(fixture.worktree.join("omit/two.txt"), "vivified\n").unwrap();

    let outcome = fixture.release().unwrap();
    let reference = outcome.saved_to.expect("vivified state has a salvage ref");

    assert_eq!(
        show(&fixture.repo, &reference, "omit/two.txt"),
        b"vivified\n"
    );
    assert_eq!(
        show(&fixture.repo, &reference, "omit/three.txt"),
        b"three\n"
    );
    assert_eq!(
        show(&fixture.repo, &fixture.branch, "omit/three.txt"),
        b"three\n"
    );
}

#[test]
fn concurrent_index_lock_blocks_cleanup_before_any_salvage_ref() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join("tracked.txt"), "dirty\n").unwrap();
    let lock = index_path(&fixture.worktree).with_file_name("index.lock");
    fs::write(&lock, "concurrent writer").unwrap();

    let error = fixture.release_error();

    assert!(error.to_string().contains("Git index is locked"));
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("tracked.txt")).unwrap(),
        "dirty\n"
    );
    assert!(salvage_refs(&fixture.repo).is_empty());
    fs::remove_file(lock).unwrap();
}
