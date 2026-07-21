use super::{Request, acquire, load};
use crate::snapshot;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    source: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let root = temp.path().join("state");
        fs::create_dir(&source).unwrap();
        git(&source, &["init"]);
        git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&source, &["config", "user.name", "Grove Test"]);
        git(&source, &["config", "user.email", "grove@example.invalid"]);
        git(&source, &["config", "credential.helper", "!exit 1"]);
        git(
            &source,
            &[
                "config",
                "http.https://example.invalid/.extraheader",
                "Authorization: secret",
            ],
        );
        write(&source, "mixed.bin", b"base\0bytes");
        write(&source, "deleted.txt", b"delete me");
        write(&source, "plain.txt", b"plain");
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "base"]);
        Self {
            _temp: temp,
            source,
            root: fs::canonicalize(root.parent().unwrap())
                .unwrap()
                .join("state"),
        }
    }

    fn request<'a>(&'a self, capsule_id: &'a str) -> Request<'a> {
        Request {
            root: &self.root,
            workspace: &self.source,
            task_id: "task-inspect-1",
            capsule_id,
            expires_at: now() + 3_600,
        }
    }
}

#[test]
fn exact_dirty_capsule_is_independent_from_source_git() {
    let fixture = Fixture::new();
    let secret_blob = secret_blob(&fixture.source);
    write(&fixture.source, "mixed.bin", b"staged\0binary");
    write(&fixture.source, "added.txt", b"staged addition");
    git(&fixture.source, &["add", "mixed.bin", "added.txt"]);
    write(&fixture.source, "mixed.bin", b"working\0binary");
    write(&fixture.source, "added.txt", b"working addition");
    fs::remove_file(fixture.source.join("deleted.txt")).unwrap();
    write(&fixture.source, "odd snowman \u{2603}.txt", b"untracked");

    let branch = capture(&fixture.source, &["symbolic-ref", "HEAD"]);
    let head = capture(&fixture.source, &["rev-parse", "HEAD"]);
    let status = bytes(&fixture.source, &["status", "--porcelain=v1", "-z"]);
    let index_before_capture = fs::read(git_index(&fixture.source)).unwrap();
    let objects_before_capture = object_paths(&fixture.source);
    let before = snapshot::capture_read_only(&fixture.source).unwrap();
    assert_eq!(
        fs::read(git_index(&fixture.source)).unwrap(),
        index_before_capture
    );
    assert_eq!(object_paths(&fixture.source), objects_before_capture);
    let request = fixture.request("capsule-rich");
    let capsule = acquire(&request).unwrap();
    let source_objects = object_paths(&fixture.source);

    assert!(snapshot::capture(&capsule.path).unwrap() == before);
    assert_eq!(
        bytes(&capsule.path, &["status", "--porcelain=v1", "-z"]),
        status
    );
    assert_eq!(load(&request, &before.sha256).unwrap(), capsule.binding);
    assert_git_is_private(&fixture.source, &capsule.path);
    assert!(!success(&capsule.path, &["cat-file", "-e", &secret_blob]));

    mutate_capsule(&capsule.path, &head);
    assert!(snapshot::capture_read_only(&fixture.source).unwrap() == before);
    assert_eq!(capture(&fixture.source, &["symbolic-ref", "HEAD"]), branch);
    assert_eq!(capture(&fixture.source, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        bytes(&fixture.source, &["status", "--porcelain=v1", "-z"]),
        status
    );
    assert_eq!(object_paths(&fixture.source), source_objects);
    assert_eq!(
        fs::read(git_index(&fixture.source)).unwrap(),
        index_before_capture
    );
}

#[cfg(unix)]
#[test]
fn preserves_modes_safe_links_and_newline_names() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fixture = Fixture::new();
    write(&fixture.source, "run.sh", b"#!/bin/sh\nexit 0\n");
    fs::set_permissions(
        fixture.source.join("run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("plain.txt", fixture.source.join("plain-link")).unwrap();
    symlink("plain-link", fixture.source.join("chain-link")).unwrap();
    write(&fixture.source, "line\nbreak.txt", b"newline path");

    let before = snapshot::capture_read_only(&fixture.source).unwrap();
    let capsule = acquire(&fixture.request("capsule-unix")).unwrap();

    assert!(snapshot::capture(&capsule.path).unwrap() == before);
    assert_eq!(
        fs::metadata(capsule.path.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_link(capsule.path.join("plain-link")).unwrap(),
        Path::new("plain.txt")
    );
    assert_eq!(fs::read(capsule.path.join("chain-link")).unwrap(), b"plain");
    assert_eq!(
        fs::read(capsule.path.join("line\nbreak.txt")).unwrap(),
        b"newline path"
    );
}

#[test]
fn rejects_unborn_and_submodule_repositories() {
    let unborn = TempDir::new().unwrap();
    git(unborn.path(), &["init"]);
    let root = TempDir::new().unwrap();
    let root_path = fs::canonicalize(root.path()).unwrap();
    let request = Request {
        root: &root_path,
        workspace: unborn.path(),
        task_id: "task",
        capsule_id: "unborn",
        expires_at: now() + 60,
    };
    assert!(acquire(&request).is_err());

    let fixture = Fixture::new();
    let child = child_repo(fixture._temp.path());
    git(
        &fixture.source,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child.to_str().unwrap(),
            "vendor",
        ],
    );
    git(&fixture.source, &["commit", "-am", "add submodule"]);
    assert!(acquire(&fixture.request("initialized-submodule")).is_err());
    git(
        &fixture.source,
        &["submodule", "deinit", "-f", "--", "vendor"],
    );
    assert!(acquire(&fixture.request("uninitialized-submodule")).is_err());
}

#[test]
fn rejects_state_root_inside_source_without_writing_it() {
    let fixture = Fixture::new();
    let inside = fixture.source.join("capsule-state");
    let request = Request {
        root: &inside,
        workspace: &fixture.source,
        task_id: "task",
        capsule_id: "inside",
        expires_at: now() + 60,
    };

    assert!(acquire(&request).is_err());
    assert!(!inside.exists());
}

#[cfg(unix)]
#[test]
fn rejects_escaping_links_unsafe_names_and_special_files() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    let link = Fixture::new();
    write(link._temp.path(), "outside.txt", b"outside");
    symlink("../outside.txt", link.source.join("escape")).unwrap();
    assert!(acquire(&link.request("unsafe-link")).is_err());

    let name = Fixture::new();
    write(&name.source, "bad\\name", b"unsafe on Windows");
    assert!(acquire(&name.request("unsafe-name")).is_err());

    let special = Fixture::new();
    let _socket = UnixListener::bind(special.source.join("socket")).unwrap();
    assert!(acquire(&special.request("special-file")).is_err());
}

fn child_repo(parent: &Path) -> PathBuf {
    let child = parent.join("child");
    fs::create_dir(&child).unwrap();
    git(&child, &["init"]);
    git(&child, &["config", "user.name", "Grove Test"]);
    git(&child, &["config", "user.email", "grove@example.invalid"]);
    write(&child, "child.txt", b"child");
    git(&child, &["add", "."]);
    git(&child, &["commit", "-m", "child"]);
    child
}

fn mutate_capsule(capsule: &Path, source_head: &str) {
    git(capsule, &["config", "user.name", "Reviewer"]);
    git(
        capsule,
        &["config", "user.email", "reviewer@example.invalid"],
    );
    write(capsule, "reviewer.txt", b"review mutation");
    git(capsule, &["add", "-A"]);
    git(capsule, &["commit", "-m", "review mutation"]);
    git(capsule, &["switch", "-c", "reviewer-branch"]);
    git(capsule, &["reset", "--hard", source_head]);
}

fn secret_blob(source: &Path) -> String {
    git(source, &["checkout", "-b", "secret-branch"]);
    write(source, "secret-only.txt", b"must not enter capsule");
    git(source, &["add", "secret-only.txt"]);
    git(source, &["commit", "-m", "secret branch"]);
    let oid = capture(source, &["rev-parse", "HEAD:secret-only.txt"]);
    git(source, &["checkout", "main"]);
    oid
}

fn assert_git_is_private(source: &Path, capsule: &Path) {
    let source_git = canonical_git(source);
    let capsule_git = canonical_git(capsule);
    assert_ne!(source_git, capsule_git);
    assert!(capsule_git.starts_with(capsule));
    assert!(capture(capsule, &["remote"]).is_empty());
    assert!(capture(capsule, &["for-each-ref", "--format=%(refname)"]).is_empty());
    assert!(!capsule_git.join("objects/info/alternates").exists());
    assert!(!capsule_git.join("logs").exists());
    assert!(!capsule_git.join("packed-refs").exists());
    let config = fs::read_to_string(capsule_git.join("config")).unwrap();
    assert!(!config.contains(source.to_string_lossy().as_ref()));
    assert!(!config.to_ascii_lowercase().contains("credential"));
    assert!(!config.to_ascii_lowercase().contains("extraheader"));
    assert_ne!(
        fs::canonicalize(source_git.join("index")).unwrap(),
        fs::canonicalize(capsule_git.join("index")).unwrap()
    );
}

fn canonical_git(repo: &Path) -> PathBuf {
    let value = capture(repo, &["rev-parse", "--git-common-dir"]);
    let path = Path::new(&value);
    fs::canonicalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    })
    .unwrap()
}

fn git_index(repo: &Path) -> PathBuf {
    let value = capture(repo, &["rev-parse", "--git-path", "index"]);
    let path = Path::new(&value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}

fn object_paths(repo: &Path) -> Vec<PathBuf> {
    let root = canonical_git(repo).join("objects");
    let mut paths = Vec::new();
    collect_files(&root, &root, &mut paths);
    paths.sort();
    paths
}

fn collect_files(root: &Path, dir: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(root, &path, paths);
        } else {
            paths.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let output = command(dir, args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn capture(dir: &Path, args: &[&str]) -> String {
    let output = command(dir, args).output().unwrap();
    assert_success(args, &output);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = command(dir, args).output().unwrap();
    assert_success(args, &output);
    output.stdout
}

fn success(dir: &Path, args: &[&str]) -> bool {
    command(dir, args).output().unwrap().status.success()
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    command
}

fn assert_success(args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[path = "inspection_snapshot_hardening_tests.rs"]
mod hardening;
