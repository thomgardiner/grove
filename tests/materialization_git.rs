#![allow(clippy::unwrap_used, dead_code)]

use grove::materialization_git::{self, Add, Failure};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{TempDir, tempdir};

struct Repo {
    _dir: TempDir,
    root: PathBuf,
    base: String,
}

impl Repo {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "grove-test"]);
        write_tree(&root);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "fixture"]);
        let base = git_out(&root, &["rev-parse", "HEAD"]);
        Self {
            _dir: dir,
            root,
            base,
        }
    }

    fn add(&self, branch: &str, name: &str, checkout: bool) -> PathBuf {
        let workspace = self.root.parent().unwrap().join(name);
        materialization_git::add(&Add {
            main: &self.root,
            branch,
            existing: false,
            workspace: &workspace,
            base: &self.base,
            checkout,
        })
        .unwrap();
        workspace
    }
}

fn write(root: &Path, path: &str, value: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value).unwrap();
}

fn write_tree(root: &Path) {
    write(root, "Cargo.toml", "[workspace]\nmembers = []\n");
    write(root, "README.md", "root");
    write(root, "crates/alpha/Cargo.toml", "[package]\nname='alpha'\n");
    write(root, "crates/alpha/src/lib.rs", "pub fn alpha() {}");
    write(root, "crates/alpha/payload/large.bin", "alpha payload");
    write(root, "crates/beta/Cargo.toml", "[package]\nname='beta'\n");
    write(root, "crates/beta/src/lib.rs", "pub fn beta() {}");
    write(root, "crates/beta/payload/large.bin", "beta payload");
    write(root, "crates/with space/src/lib.rs", "pub fn spaced() {}");
    write(root, "crates/日本語/src/lib.rs", "pub fn unicode() {}");
    #[cfg(unix)]
    std::os::unix::fs::symlink("crates/alpha", root.join("alpha-link")).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn sparse_keeps_parent_manifests_and_omits_nested_payloads() {
    let repo = Repo::new();
    let workspace = repo.add("grove/sparse", "sparse", false);

    let cones = materialization_git::sparse(
        &workspace,
        &["crates/alpha".into(), "crates/beta/src".into()],
    )
    .unwrap();

    assert_eq!(cones, ["crates/alpha", "crates/beta/src"]);
    assert!(workspace.join("Cargo.toml").exists());
    assert!(workspace.join("crates/alpha/payload/large.bin").exists());
    assert!(workspace.join("crates/beta/Cargo.toml").exists());
    assert!(workspace.join("crates/beta/src/lib.rs").exists());
    assert!(!workspace.join("crates/beta/payload/large.bin").exists());
    assert_eq!(materialization_git::head(&workspace).unwrap(), repo.base);
}

#[test]
fn sparse_accepts_spaces_and_unicode_but_rejects_pseudo_directories() {
    let repo = Repo::new();
    let workspace = repo.add("grove/names", "names", false);

    for cones in [
        Vec::new(),
        vec![".".into()],
        vec!["../crates".into()],
        vec!["/crates".into()],
        vec!["crates\\alpha".into()],
        vec!["Cargo.toml".into()],
        vec!["missing/directory".into()],
    ] {
        assert!(materialization_git::sparse(&workspace, &cones).is_err());
    }
    #[cfg(unix)]
    assert!(materialization_git::sparse(&workspace, &["alpha-link".into()]).is_err());

    let cones = materialization_git::sparse(
        &workspace,
        &["crates/with space".into(), "crates/日本語".into()],
    )
    .unwrap();
    assert_eq!(cones, ["crates/with space", "crates/日本語"]);
    assert!(workspace.join("crates/with space/src/lib.rs").exists());
    assert!(workspace.join("crates/日本語/src/lib.rs").exists());
}

#[test]
fn linked_worktrees_keep_independent_cones_and_full_restores_files() {
    let repo = Repo::new();
    let alpha = repo.add("grove/alpha", "alpha", false);
    let beta = repo.add("grove/beta", "beta", false);

    assert_eq!(
        materialization_git::sparse(&alpha, &["crates/alpha".into()]).unwrap(),
        ["crates/alpha"]
    );
    assert_eq!(
        materialization_git::sparse(&beta, &["crates/beta/src".into()]).unwrap(),
        ["crates/beta/src"]
    );
    assert_eq!(
        git_out(&alpha, &["config", "--worktree", "--get", "index.sparse"]),
        "false"
    );
    assert_eq!(
        git_out(
            &alpha,
            &["config", "--worktree", "--get", "core.sparseCheckout"]
        ),
        "true"
    );
    assert_eq!(
        git_out(
            &beta,
            &["config", "--worktree", "--get", "core.sparseCheckoutCone"]
        ),
        "true"
    );
    assert_eq!(
        git_out(&beta, &["sparse-checkout", "list"]),
        "crates/beta/src"
    );
    assert!(!alpha.join("crates/beta/payload/large.bin").exists());
    assert!(!beta.join("crates/alpha/payload/large.bin").exists());

    materialization_git::full(&alpha).unwrap();

    assert!(alpha.join("crates/beta/payload/large.bin").exists());
    assert!(!beta.join("crates/alpha/payload/large.bin").exists());
    assert_eq!(
        git_out(&alpha, &["config", "--worktree", "--get", "index.sparse"]),
        "false"
    );
    assert_eq!(
        git_out(&beta, &["sparse-checkout", "list"]),
        "crates/beta/src"
    );
    materialization_git::full(&alpha).unwrap();
    assert!(alpha.join("crates/beta/payload/large.bin").exists());
}

#[test]
fn full_repairs_skip_worktree_entries_when_sparse_config_is_false() {
    let repo = Repo::new();
    let workspace = repo.add("grove/repair", "repair", false);
    materialization_git::sparse(&workspace, &["crates/alpha".into()]).unwrap();
    assert!(!workspace.join("crates/beta/payload/large.bin").exists());
    git(
        &workspace,
        &["config", "--worktree", "core.sparseCheckout", "false"],
    );
    assert!(
        git_out(&workspace, &["ls-files", "-t"])
            .lines()
            .any(|entry| entry.starts_with("S "))
    );

    materialization_git::full(&workspace).unwrap();

    assert!(workspace.join("crates/beta/payload/large.bin").is_file());
    assert!(
        !git_out(&workspace, &["ls-files", "-t"])
            .lines()
            .any(|entry| entry.starts_with("S "))
    );
    assert_eq!(
        git_out(
            &workspace,
            &["config", "--worktree", "--get", "core.sparseCheckout"]
        ),
        "false"
    );
}

#[test]
fn full_clears_stale_sparse_index_configuration() {
    let repo = Repo::new();
    let workspace = repo.add("grove/index-config", "index-config", false);
    materialization_git::sparse(&workspace, &["crates/alpha".into()]).unwrap();
    materialization_git::full(&workspace).unwrap();
    git(
        &workspace,
        &["config", "--worktree", "index.sparse", "true"],
    );

    materialization_git::full(&workspace).unwrap();

    assert_eq!(
        git_out(
            &workspace,
            &["config", "--worktree", "--get", "index.sparse"]
        ),
        "false"
    );
}

#[test]
fn sparse_checkout_preserves_main_worktree_specific_configuration() {
    let repo = Repo::new();
    let main = fs::canonicalize(&repo.root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    git(&repo.root, &["config", "core.worktree", &main]);
    let workspace = repo.add("grove/config", "config", false);

    materialization_git::sparse(&workspace, &["crates/alpha".into()]).unwrap();

    assert_eq!(
        fs::canonicalize(git_out(&repo.root, &["rev-parse", "--show-toplevel"])).unwrap(),
        fs::canonicalize(&repo.root).unwrap()
    );
    assert_eq!(
        fs::canonicalize(git_out(
            &repo.root,
            &["config", "--worktree", "--get", "core.worktree"]
        ))
        .unwrap(),
        fs::canonicalize(&repo.root).unwrap()
    );
}

#[test]
fn add_supports_full_no_checkout_and_existing_branches() {
    let repo = Repo::new();
    let empty = repo.add("grove/empty", "empty", false);
    assert!(!empty.join("README.md").exists());

    git(&repo.root, &["branch", "grove/existing", &repo.base]);
    let full = repo.root.parent().unwrap().join("full");
    materialization_git::add(&Add {
        main: &repo.root,
        branch: "grove/existing",
        existing: true,
        workspace: &full,
        base: &repo.base,
        checkout: true,
    })
    .unwrap();

    assert!(full.join("README.md").exists());
    assert_eq!(materialization_git::head(&full).unwrap(), repo.base);
    materialization_git::full(&full).unwrap();
    materialization_git::full(&empty).unwrap();
    assert!(empty.join("crates/beta/payload/large.bin").exists());
}

#[test]
fn unsupported_git_diagnostics_are_classified_for_fallback() {
    let failure = Failure::classify(
        Some(129),
        "error: unknown option `no-sparse-index`\nusage: git sparse-checkout",
    );
    assert!(matches!(failure, Failure::Unsupported(_)));

    let failure = Failure::classify(Some(1), "fatal: unable to update working tree");
    assert!(matches!(failure, Failure::Setup(_)));
}

#[cfg(windows)]
#[test]
fn add_accepts_a_canonical_verbatim_workspace_path() {
    let repo = Repo::new();
    let parent = fs::canonicalize(repo.root.parent().unwrap()).unwrap();
    let workspace = parent.join("verbatim-worktree");

    materialization_git::add(&Add {
        main: &repo.root,
        branch: "grove/verbatim",
        existing: false,
        workspace: &workspace,
        base: &repo.base,
        checkout: true,
    })
    .unwrap();

    assert!(workspace.join("README.md").is_file());
    assert_eq!(materialization_git::head(&workspace).unwrap(), repo.base);
}
