//! Integration tests for the cache and copy-on-write seeding, against real temp
//! directories (no mocks). The clone benchmark is `#[ignore]`d; run it against a real
//! target with `GROVE_BENCH_SRC=/path/to/target cargo test --release bench -- --ignored --nocapture`.

use grove::{cache, seed, worktree};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::tempdir;
#[cfg(unix)]
fn pool_tokens(root: &Path) -> usize {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;

    let mut fifo = std::fs::File::from(
        rustix::fs::open(
            root.join("jobserver"),
            OFlags::RDWR | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .unwrap(),
    );
    let mut total = 0;
    let mut buf = [0; 64];
    loop {
        match fifo.read(&mut buf) {
            Ok(0) => return total,
            Ok(read) => total += read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return total,
            Err(error) => panic!("reading jobserver tokens: {error}"),
        }
    }
}
#[cfg(unix)]
fn workspace(base: &Path, name: &str, slots: usize) -> std::path::PathBuf {
    let workspace = base.join(name);
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        workspace.join(".grove.toml"),
        format!("cpu_slots = {slots}\n"),
    )
    .unwrap();
    workspace
}

#[test]
fn clone_tree_reproduces_the_source_and_replaces_the_destination() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    fs::create_dir_all(src.join("a/b")).unwrap();
    fs::write(src.join("a/b/deep.txt"), b"deep").unwrap();
    fs::write(src.join("top.txt"), b"top").unwrap();
    // Pre-existing (stale) destination content must be gone after cloning.
    fs::create_dir_all(&dst).unwrap();
    fs::write(dst.join("stale.txt"), b"stale").unwrap();

    seed::clone_tree(&src, &dst).unwrap();

    assert_eq!(fs::read(dst.join("a/b/deep.txt")).unwrap(), b"deep");
    assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
    assert!(
        !dst.join("stale.txt").exists(),
        "stale destination content must be replaced"
    );
}

// APFS is copy-on-write, so a strict clone succeeds on the dev/reference machine. On a
// non-CoW volume the same call is expected to fail rather than fall back to a full copy;
// that path is filesystem-specific and not asserted here.
#[cfg(target_os = "macos")]
#[test]
fn strict_cow_clone_succeeds_on_apfs() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("final.rlib"), b"artifact").unwrap();

    seed::clone_tree_cow(&src, &dst, true).unwrap();

    assert_eq!(fs::read(dst.join("final.rlib")).unwrap(), b"artifact");
}

#[test]
fn lane_ids_are_stable_and_specific() {
    assert_eq!(
        cache::lane_id("/repo", "stable"),
        cache::lane_id("/repo", "stable")
    );
    assert_ne!(
        cache::lane_id("/repo/a", "stable"),
        cache::lane_id("/repo/b", "stable")
    );
    assert_ne!(
        cache::lane_id("/repo", "stable"),
        cache::lane_id("/repo", "nightly")
    );
}

#[cfg(unix)]
#[test]
fn lane_owned_governors_isolate_roots_and_resize_only_after_idle() {
    unsafe {
        // SAFETY: nextest runs each test in its own process.
        std::env::remove_var("GROVE_GOVERNOR_MODE");
        std::env::remove_var("GROVE_CPU_SLOTS");
        std::env::remove_var("GROVE_MAX_BUILDERS");
    }
    let base = tempdir().unwrap();
    let root_a = base.path().join("cache-a");
    let root_b = base.path().join("cache-b");
    let a_two = workspace(base.path(), "a-two", 2);
    let a_nine = workspace(base.path(), "a-nine", 9);
    let a_three = workspace(base.path(), "a-three", 3);
    let b_four = workspace(base.path(), "b-four", 4);
    let lane_a = cache::acquire(&root_a, &a_two.to_string_lossy(), "stable").unwrap();
    let lane_b = cache::acquire(&root_b, &b_four.to_string_lossy(), "stable").unwrap();
    assert_eq!(pool_tokens(&root_a), 1);
    assert_eq!(pool_tokens(&root_b), 3);

    let mut command_a = Command::new("cargo");
    let mut command_b = Command::new("cargo");
    cache::apply_env(&mut command_a, &lane_a);
    cache::apply_env(&mut command_b, &lane_b);
    let flags = |command: &Command| {
        command
            .get_envs()
            .find(|(key, _)| *key == "MAKEFLAGS")
            .and_then(|(_, value)| value)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    assert!(flags(&command_a).contains(&root_a.join("jobserver").to_string_lossy().to_string()));
    assert!(flags(&command_b).contains(&root_b.join("jobserver").to_string_lossy().to_string()));

    let active_resize = cache::acquire(&root_a, &a_nine.to_string_lossy(), "stable").unwrap();
    assert_eq!(pool_tokens(&root_a), 0, "an active pool is never refilled");
    drop(lane_a);
    drop(active_resize);

    let idle_resize = cache::acquire(&root_a, &a_three.to_string_lossy(), "stable").unwrap();
    assert_eq!(
        pool_tokens(&root_a),
        2,
        "the first idle joiner resizes the pool"
    );
    drop(idle_resize);
    drop(lane_b);
}

#[test]
fn seed_clones_a_cold_lane_and_leaves_a_warm_one_alone() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let ws_str = ws.path().to_string_lossy().into_owned();

    // A canonical holding one target artifact.
    let canonical = cache::canonical_dir(root.path(), &ws_str, "stable");
    fs::create_dir_all(canonical.join("target")).unwrap();
    fs::write(canonical.join("target/libengine.rmeta"), b"seed").unwrap();

    let lane = cache::acquire(root.path(), &ws_str, "stable").unwrap();
    assert!(!lane.target_dir.exists(), "a fresh lane is cold");
    assert!(
        cache::seed(root.path(), &lane, &canonical).unwrap(),
        "a cold lane with a canonical seeds"
    );
    assert_eq!(
        fs::read(lane.target_dir.join("libengine.rmeta")).unwrap(),
        b"seed"
    );

    assert!(
        !cache::seed(root.path(), &lane, &canonical).unwrap(),
        "a warm lane is left untouched"
    );
}

#[test]
fn promote_captures_the_whole_lane_into_the_canonical() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let ws_str = ws.path().to_string_lossy().into_owned();

    let lane = cache::acquire(root.path(), &ws_str, "stable").unwrap();
    fs::create_dir_all(&lane.build_dir).unwrap();
    fs::create_dir_all(&lane.target_dir).unwrap();
    fs::write(lane.build_dir.join("intermediate.o"), b"obj").unwrap();
    fs::write(lane.target_dir.join("final.rlib"), b"lib").unwrap();

    let canonical = cache::canonical_dir(root.path(), &ws_str, "stable");
    cache::promote(root.path(), &lane, &canonical).unwrap();

    assert_eq!(
        fs::read(canonical.join("build/intermediate.o")).unwrap(),
        b"obj"
    );
    assert_eq!(
        fs::read(canonical.join("target/final.rlib")).unwrap(),
        b"lib"
    );
}

#[test]
fn concurrent_tags_reuse_one_unverified_bootstrap_until_a_canonical_exists() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let root = base.path().join("cache");
    let build_log = base.path().join("build.log");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='bootstrap_fixture'\nversion='0.1.0'\nedition='2024'\nbuild='build.rs'\n",
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn ready() {}\n").unwrap();
    fs::write(
        repo.join("build.rs"),
        "use std::io::Write;\nfn main() { let path = std::env::var_os(\"GROVE_TEST_BUILD_LOG\").unwrap(); let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap(); writeln!(f, \"built\").unwrap(); }\n",
    )
    .unwrap();
    assert!(
        Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "grove@example.test"]);
    git(&["config", "user.name", "Grove Test"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "fixture"]);

    let workspace = fs::canonicalize(&repo).unwrap();
    let workspace_str = workspace.to_string_lossy().into_owned();
    let repo_git = fs::canonicalize(repo.join(".git"))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let branch = String::from_utf8(
        Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    fs::create_dir_all(root.join("leases")).unwrap();
    let lease_path = root
        .join("leases")
        .join(format!("{}.json", cache::lane_id(&workspace_str, "stable")));
    fs::write(
        &lease_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "workspace": workspace_str,
            "branch": branch.trim(),
            "agent": "bootstrap-fixture",
            "toolchain": "stable",
            "repo": repo_git,
            "created_at": 1,
            "last_activity": 1,
            "base_oid": "fixture"
        }))
        .unwrap(),
    )
    .unwrap();

    let spawn = |tag: &str| {
        Command::new(env!("CARGO_BIN_EXE_grove"))
            .args(["exec", "--tag", tag, "--", "cargo", "check", "--locked"])
            .current_dir(&repo)
            .env("GROVE_CACHE_ROOT", &root)
            .env("GROVE_TEST_BUILD_LOG", &build_log)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn("first");
    let second = spawn("second");
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(fs::read_to_string(&build_log).unwrap().lines().count(), 1);

    let repeated = spawn("first").wait_with_output().unwrap();
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(fs::read_to_string(&build_log).unwrap().lines().count(), 1);

    let grove = grove::api::Grove::with_root(root.clone(), &repo);
    assert!(
        !grove.canonical().exists(),
        "bootstrap is never canonical evidence"
    );
    let lanes = fs::read_dir(root.join("lanes"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .collect::<Vec<_>>();
    assert_eq!(lanes.len(), 1, "all missing-canonical tags share one lane");
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(lanes[0].path().join(".grove-meta.json")).unwrap())
            .unwrap();
    assert_eq!(meta["tag"], "bootstrap-unverified");
    let lease: worktree::Lease = serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
    assert!(
        lease.last_activity > 1,
        "bootstrap exec renews the managed worktree lease"
    );

    let failed = Command::new(env!("CARGO_BIN_EXE_grove"))
        .args([
            "exec",
            "--tag",
            "failed",
            "--",
            "cargo",
            "check",
            "--package",
            "missing-package",
        ])
        .current_dir(&repo)
        .env("GROVE_CACHE_ROOT", &root)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        !grove.canonical().exists(),
        "failed exec cannot publish evidence"
    );
    assert_eq!(fs::read_dir(root.join("lanes")).unwrap().count(), 1);
}

#[test]
fn reclaim_stale_drops_gone_worktrees_and_keeps_live_ones() {
    let root = tempdir().unwrap();
    let live_ws = tempdir().unwrap();
    let live_str = live_ws.path().to_string_lossy().into_owned();
    let gone_str = root
        .path()
        .join("deleted-worktree")
        .to_string_lossy()
        .into_owned();

    fs::create_dir(&gone_str).unwrap();
    let live = cache::acquire(root.path(), &live_str, "stable").unwrap();
    let gone = cache::acquire(root.path(), &gone_str, "stable").unwrap();
    let (live_dir, gone_dir) = (live.dir.clone(), gone.dir.clone());
    drop(live);
    drop(gone); // release locks so GC can claim them
    fs::remove_dir(&gone_str).unwrap();

    let reclaimed = cache::reclaim_stale(root.path());

    assert!(live_dir.exists(), "a live worktree's lane is kept");
    assert!(!gone_dir.exists(), "a gone worktree's lane is reclaimed");
    assert_eq!(reclaimed.len(), 1);
}

#[test]
#[ignore = "benchmark; needs GROVE_BENCH_SRC pointing at a real target dir on the same volume"]
fn bench_clone_large_tree() {
    let src = std::env::var("GROVE_BENCH_SRC").expect("set GROVE_BENCH_SRC");
    let dst = format!("{src}-grove-bench");
    let _ = fs::remove_dir_all(&dst);
    let started = std::time::Instant::now();
    seed::clone_tree(Path::new(&src), Path::new(&dst)).unwrap();
    let elapsed = started.elapsed();
    let _ = fs::remove_dir_all(&dst);
    eprintln!("clone_tree of {src} took {elapsed:?}");
}
