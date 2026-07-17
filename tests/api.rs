//! Integration tests for the `Grove` facade against real temp directories. An empty temp
//! dir is not a cargo project, so `project::workspace` falls back to the dir itself and no
//! git repo is needed — enough to exercise the resolve → canonical → seed wiring.

use grove::api::Grove;
use grove::cache;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

struct Cwd(PathBuf);

impl Drop for Cwd {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).unwrap();
    }
}

struct Env {
    key: &'static str,
    value: Option<OsString>,
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(value) = &self.value {
            // SAFETY: nextest runs each test in its own process.
            unsafe { std::env::set_var(self.key, value) };
        } else {
            unsafe { std::env::remove_var(self.key) };
        }
    }
}

fn without_env(key: &'static str) -> Env {
    let value = std::env::var_os(key);
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::remove_var(key) };
    Env { key, value }
}

fn toml_string(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[test]
fn facade_resolves_the_workspace_and_seeds_a_lane_from_the_canonical() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), ws.path());

    // The workspace is the resolved (symlinks-followed) path, matching what prewarm keys.
    assert_eq!(grove.workspace(), cache::canonical_path(ws.path()));

    // Put an artifact in the canonical this facade resolves, then seed a lane from it.
    let canonical = grove.canonical();
    fs::create_dir_all(canonical.join("target")).unwrap();
    fs::write(canonical.join("target/libengine.rmeta"), b"seed").unwrap();

    let lane = grove.seeded_lane().unwrap();
    assert_eq!(
        fs::read(lane.target_dir.join("libengine.rmeta")).unwrap(),
        b"seed",
        "seeded_lane clones the canonical into the lane"
    );
}

#[test]
fn facade_promotes_a_lane_into_the_canonical() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let grove = Grove::with_root(root.path().to_path_buf(), ws.path());

    let lane = grove.lane().unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.target_dir.join("final.rlib"), b"built").unwrap();

    grove.promote(&lane).unwrap();

    assert_eq!(
        fs::read(grove.canonical().join("target/final.rlib")).unwrap(),
        b"built",
        "promote publishes the lane as the canonical"
    );
}

#[test]
fn facade_keys_direct_cargo_toolchain_selectors() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let command = vec!["cargo".into(), "+nightly".into(), "check".into()];

    let grove = Grove::with_root_for_command(root.path().to_path_buf(), ws.path(), &command);

    assert_eq!(grove.toolchain(), "nightly");
}

#[test]
fn handles_keep_repository_configuration_in_either_open_order() {
    let _env = without_env("GROVE_CACHE_ROOT");
    let _cwd = Cwd(std::env::current_dir().unwrap());
    let base = tempdir().unwrap();
    let repo_a = base.path().join("repo-a");
    let repo_b = base.path().join("repo-b");
    let root_a = base.path().join("cache-a");
    let root_b = base.path().join("cache-b");
    for (repo, root) in [(&repo_a, &root_a), (&repo_b, &root_b)] {
        fs::create_dir_all(repo).unwrap();
        fs::write(
            repo.join(".grove.toml"),
            format!("cache_root = \"{}\"\n", toml_string(root)),
        )
        .unwrap();
    }

    std::env::set_current_dir(&repo_b).unwrap();
    let a_first = Grove::open(&repo_a);
    std::env::set_current_dir(&repo_a).unwrap();
    let b_second = Grove::open(&repo_b);
    std::env::set_current_dir(&repo_a).unwrap();
    let b_first = Grove::open(&repo_b);
    std::env::set_current_dir(&repo_b).unwrap();
    let a_second = Grove::open(&repo_a);

    for handle in [&a_first, &a_second] {
        assert_eq!(handle.root(), root_a);
        assert_eq!(handle.workspace(), cache::canonical_path(&repo_a));
    }
    for handle in [&b_first, &b_second] {
        assert_eq!(handle.root(), root_b);
        assert_eq!(handle.workspace(), cache::canonical_path(&repo_b));
    }
}

#[test]
fn gc_reports_each_handles_bound_disk_policy() {
    let _min = without_env("GROVE_MIN_FREE_GB");
    let _max = without_env("GROVE_MAX_CANONICAL_GB");
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let repo_a = base.path().join("repo-a");
    let repo_b = base.path().join("repo-b");
    for (repo, budget) in [(&repo_a, 1_000), (&repo_b, 2_000)] {
        fs::create_dir_all(repo).unwrap();
        fs::write(
            repo.join(".grove.toml"),
            format!("min_free_gb = 0\nmax_canonical_gb = {budget}\n"),
        )
        .unwrap();
    }

    let grove_a = Grove::with_root(root.clone(), &repo_a);
    let grove_b = Grove::with_root(root, &repo_b);
    fs::write(
        repo_a.join(".grove.toml"),
        "min_free_gb = 0\nmax_canonical_gb = 3\n",
    )
    .unwrap();
    fs::write(
        repo_b.join(".grove.toml"),
        "min_free_gb = 0\nmax_canonical_gb = 4\n",
    )
    .unwrap();

    let gib = 1024 * 1024 * 1024;
    let report_a = grove_a.gc();
    let report_b = grove_b.gc();
    assert_eq!(report_a.floor_bytes, 0);
    assert_eq!(report_b.floor_bytes, 0);
    assert_eq!(report_a.canonical_budget_bytes, Some(1_000 * gib));
    assert_eq!(report_b.canonical_budget_bytes, Some(2_000 * gib));
    assert_eq!(grove_a.maintain(|| 7), 7);
}
