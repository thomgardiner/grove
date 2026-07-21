use grove::snapshot::{self, Kind};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

fn git(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn repo() -> tempfile::TempDir {
    let repo = tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "core.autocrlf", "false"]);
    git(
        repo.path(),
        &["config", "user.email", "sparse@example.test"],
    );
    git(repo.path(), &["config", "user.name", "sparse-test"]);
    for (path, content) in [
        ("selected/in.txt", "selected\n"),
        ("outside/file.txt", "outside\n"),
        ("omitted/file.txt", "omitted\n"),
    ] {
        let path = repo.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    repo
}

fn sparse(repo: &Path, cones: &[&str]) {
    git(
        repo,
        &["sparse-checkout", "init", "--cone", "--no-sparse-index"],
    );
    let mut args = vec!["sparse-checkout", "set"];
    args.extend_from_slice(cones);
    git(repo, &args);
}

fn grove(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(GROVE)
        .args(args)
        .current_dir(repo)
        .env("GROVE_CACHE_ROOT", cache)
        .output()
        .unwrap()
}

fn begin(repo: &Path, cache: &Path) -> String {
    let output = grove(
        repo,
        cache,
        &[
            "task",
            "begin",
            "--agent",
            "sparse-agent",
            "--task",
            "sparse-scope",
            "--scope",
            "selected",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    report["task"]["id"].as_str().unwrap().to_string()
}

fn finish(repo: &Path, cache: &Path, id: &str) -> Output {
    grove(
        repo,
        cache,
        &[
            "task",
            "finish",
            "--task-id",
            id,
            "--allow-unverified",
            "scope regression",
        ],
    )
}

#[test]
fn snapshot_identity_survives_sparse_expansion_and_full_materialization() {
    let repo = repo();
    let full = snapshot::capture(repo.path()).unwrap();
    sparse(repo.path(), &["selected"]);
    let sparse = snapshot::capture(repo.path()).unwrap();

    assert!(sparse == full);
    assert!(sparse.entries.iter().any(|entry| {
        entry.path == "omitted/file.txt" && entry.kind == Kind::File && entry.sha256.is_some()
    }));

    let cache = tempdir().unwrap();
    let id = begin(repo.path(), cache.path());
    git(repo.path(), &["sparse-checkout", "add", "outside"]);
    let finished = finish(repo.path(), cache.path(), &id);
    assert!(
        finished.status.success(),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );

    git(repo.path(), &["sparse-checkout", "disable"]);
    assert!(snapshot::capture(repo.path()).unwrap() == full);
}

#[test]
fn sparse_snapshot_reads_staged_bytes_from_the_index() {
    let repo = repo();
    sparse(repo.path(), &["selected"]);
    let before = snapshot::capture(repo.path()).unwrap();
    let source = repo.path().join("selected/in.txt");
    fs::write(&source, "staged\n").unwrap();
    let oid = String::from_utf8(git(repo.path(), &["hash-object", "-w", "selected/in.txt"]).stdout)
        .unwrap();
    fs::write(source, "selected\n").unwrap();
    git(
        repo.path(),
        &[
            "update-index",
            "--cacheinfo",
            "100644",
            oid.trim(),
            "omitted/file.txt",
        ],
    );
    git(
        repo.path(),
        &["update-index", "--skip-worktree", "omitted/file.txt"],
    );
    let staged = snapshot::capture(repo.path()).unwrap();
    assert!(staged != before);

    git(repo.path(), &["sparse-checkout", "add", "omitted"]);
    assert_eq!(
        fs::read(repo.path().join("omitted/file.txt")).unwrap(),
        b"staged\n"
    );
    assert!(snapshot::capture(repo.path()).unwrap() == staged);
}

#[test]
fn sparse_snapshot_applies_checkout_filters_to_index_bytes() {
    let repo = repo();
    fs::write(
        repo.path().join(".gitattributes"),
        "omitted/filtered.txt text eol=crlf\n",
    )
    .unwrap();
    fs::write(repo.path().join("omitted/filtered.txt"), "filtered\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "filtered fixture"]);
    fs::remove_file(repo.path().join("omitted/filtered.txt")).unwrap();
    git(repo.path(), &["restore", "omitted/filtered.txt"]);
    assert_eq!(
        fs::read(repo.path().join("omitted/filtered.txt")).unwrap(),
        b"filtered\r\n"
    );
    let full = snapshot::capture(repo.path()).unwrap();

    sparse(repo.path(), &["selected"]);
    let sparse = snapshot::capture(repo.path()).unwrap();
    assert!(sparse == full);

    git(repo.path(), &["sparse-checkout", "add", "omitted"]);
    assert_eq!(
        fs::read(repo.path().join("omitted/filtered.txt")).unwrap(),
        b"filtered\r\n"
    );
    assert!(snapshot::capture(repo.path()).unwrap() == sparse);
}

#[test]
fn genuine_out_of_scope_deletion_remains_visible() {
    let repo = repo();
    sparse(repo.path(), &["selected", "outside"]);
    let cache = tempdir().unwrap();
    let id = begin(repo.path(), cache.path());
    fs::remove_file(repo.path().join("outside/file.txt")).unwrap();

    let deleted = snapshot::capture(repo.path()).unwrap();
    assert!(deleted.entries.iter().any(|entry| {
        entry.path == "outside/file.txt" && entry.kind == Kind::Deleted && entry.sha256.is_none()
    }));
    // Since 0.3.2 the scope refusal is a machine-readable envelope on stdout,
    // not prose on stderr; the deletion must be named there.
    let finished = finish(repo.path(), cache.path(), &id);
    assert!(!finished.status.success());
    let refusal: Value = serde_json::from_slice(&finished.stdout).unwrap();
    assert_eq!(refusal["outcome"], "refused", "{refusal}");
    assert_eq!(refusal["reason"], "scope", "{refusal}");
    assert!(
        refusal["outside_scope"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|p| p == "outside/file.txt")),
        "{refusal}"
    );
}
