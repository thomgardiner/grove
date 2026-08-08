//! Temp-repo tests for host safety and one compile-through-lane path.

use crate::{CacheHost, CleanOpts, WorktreeManager, cargo_check};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    assert!(crate::git::ok(dir, &["init", "-b", "main"]));
    assert!(crate::git::ok(
        dir,
        &["config", "user.email", "t@example.com"]
    ));
    assert!(crate::git::ok(dir, &["config", "user.name", "t"]));
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname=\"t\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/lib.rs"), "pub fn x() -> i32 { 1 }\n").unwrap();
    assert!(crate::git::ok(dir, &["add", "."]));
    assert!(crate::git::ok(dir, &["commit", "-m", "init"]));
}

struct EnvGuard {
    keys: Vec<String>,
}

impl EnvGuard {
    fn set(pairs: &[(&str, PathBuf)]) -> Self {
        let mut keys = Vec::new();
        for (k, v) in pairs {
            // SAFETY: tests serialize via ENV_LOCK.
            unsafe { std::env::set_var(k, v) };
            keys.push((*k).to_string());
        }
        Self { keys }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for k in &self.keys {
            unsafe { std::env::remove_var(k) };
        }
    }
}

#[test]
fn seed_clean_requires_force() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let _env = EnvGuard::set(&[("GROVE_CACHE", tmp.path().join("cache"))]);
    let host = CacheHost::open(&repo).unwrap();
    let seed = host.seed_target();
    fs::create_dir_all(&seed).unwrap();
    fs::write(seed.join("x"), b"1").unwrap();
    let mut opts = CleanOpts::from_env();
    opts.seed = true;
    opts.force = false;
    assert!(host.clean(&opts).is_err());
    opts.force = true;
    assert!(host.clean(&opts).unwrap().seed_removed());
}

#[test]
fn release_refuses_dirty_without_force() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let _env = EnvGuard::set(&[
        ("GROVE_CACHE", tmp.path().join("cache")),
        ("GROVE_WORKTREE_ROOT", tmp.path().join("cache").join("wts")),
    ]);
    let mgr = WorktreeManager::open(&repo).unwrap();
    let path = mgr.acquire("a1", None, None, false).unwrap();
    fs::write(path.join("dirty.txt"), b"x").unwrap();
    assert!(mgr.release(&path, false).is_err());
    assert!(mgr.release(&path, true).is_ok());
}

#[test]
fn acquire_reattaches_existing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let _env = EnvGuard::set(&[
        ("GROVE_CACHE", tmp.path().join("cache")),
        ("GROVE_WORKTREE_ROOT", tmp.path().join("cache").join("wts")),
    ]);
    let mgr = WorktreeManager::open(&repo).unwrap();
    let p1 = mgr.acquire("agent-x", None, None, false).unwrap();
    let p2 = mgr.acquire("agent-x", None, None, false).unwrap();
    assert_eq!(
        fs::canonicalize(&p1).unwrap(),
        fs::canonicalize(&p2).unwrap()
    );
    let p3 = mgr.acquire("agent-x", None, None, true).unwrap();
    assert!(p3.exists());
    assert!(mgr.release(&p3, true).is_ok());
}

#[test]
fn orphan_clean_fails_closed_on_bad_git() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let not_git = tmp.path().join("nogit");
    fs::create_dir_all(&not_git).unwrap();
    let _env = EnvGuard::set(&[("GROVE_CACHE", tmp.path().join("cache"))]);
    let host = CacheHost::open(&repo).unwrap();
    assert!(host.clean_orphan_worktrees(&not_git, true).is_err());
}

#[test]
fn orphan_clean_refuses_arbitrary_root() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    // Outside both cache and the repo-adjacent default worktree parent.
    let outside = tmp.path().join("elsewhere").join("wts");
    fs::create_dir_all(&outside).unwrap();
    let _env = EnvGuard::set(&[
        ("GROVE_CACHE", tmp.path().join("cache")),
        ("GROVE_WORKTREE_ROOT", outside),
    ]);
    let host = CacheHost::open(&repo).unwrap();
    let err = host.clean_orphan_worktrees(&repo, true).unwrap_err();
    assert!(
        err.to_string().contains("orphan clean refused"),
        "got {err:#}"
    );
}

#[test]
fn cargo_check_through_lane() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let cache = tmp.path().join("cache");
    let _env = EnvGuard::set(&[("GROVE_CACHE", cache.clone())]);
    // Drop any wrapper so this test is not sccache-coupled.
    unsafe {
        std::env::remove_var("RUSTC_WRAPPER");
        std::env::remove_var("RUSTC_WORKSPACE_WRAPPER");
    }
    let code = cargo_check(&repo, "HEAD", Some("t")).expect("cargo_check");
    assert_eq!(code, 0, "grove lane cargo check failed");
    let host = CacheHost::open(&repo).unwrap();
    let lane = host.lane(&repo).unwrap();
    assert!(lane.is_seed());
    let cache = fs::canonicalize(&cache).unwrap();
    let target = fs::canonicalize(&lane.target_dir).unwrap_or(lane.target_dir.clone());
    assert!(
        target.starts_with(&cache),
        "target {} not under cache {}",
        target.display(),
        cache.display()
    );
    assert!(
        lane.target_dir.join("debug").is_dir(),
        "lane target empty: {}",
        lane.target_dir.display()
    );
}

/// Lanes hardlink from the seed, so a shared file must be charged once per report.
/// Before this, `bytes_reclaimed` reported 148G for 50G of actually-recoverable space.
#[test]
fn reclaimed_bytes_count_hardlinks_once() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let _env = EnvGuard::set(&[("GROVE_CACHE", tmp.path().join("cache"))]);
    let host = CacheHost::open(&repo).unwrap();

    // One 4 KiB payload, hardlinked into two lanes.
    let lanes = host.host_root().join("lanes");
    let payload = vec![b'x'; 4096];
    for key in ["aaaaaaaaaaaa", "bbbbbbbbbbbb"] {
        let t = lanes.join(key).join("target");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join(".grove-access"), b"").unwrap();
        set_old(&t.join(".grove-access"));
    }
    let src = lanes.join("aaaaaaaaaaaa/target/blob");
    fs::write(&src, &payload).unwrap();
    fs::hard_link(&src, lanes.join("bbbbbbbbbbbb/target/blob")).unwrap();

    let mut opts = CleanOpts::from_env();
    opts.dry_run = true;
    opts.lane_ttl = std::time::Duration::from_secs(1);
    opts.cache_max_bytes = None;
    let report = host.clean(&opts).unwrap();

    assert_eq!(report.lanes_removed, 2);
    // 4096 counted once, not twice; small dir/stamp overhead tolerated.
    assert!(
        report.bytes_reclaimed < 2 * 4096,
        "hardlinked blob double-counted: {}",
        report.bytes_reclaimed
    );
}

/// A TTL alone cannot bound size: active lanes stay fresh forever. The ceiling
/// must evict coldest-first, and must never take a protected (live) lane.
#[test]
fn cache_ceiling_evicts_coldest_and_spares_live_lanes() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let _env = EnvGuard::set(&[("GROVE_CACHE", tmp.path().join("cache"))]);
    let host = CacheHost::open(&repo).unwrap();

    let lanes = host.host_root().join("lanes");
    // All three are fresh, so the TTL pass must not touch any of them.
    for (key, age) in [
        ("cccccccccccc", 3000),
        ("dddddddddddd", 2000),
        ("eeeeeeeeeeee", 1000),
    ] {
        let t = lanes.join(key).join("target");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("blob"), vec![b'x'; 64 * 1024]).unwrap();
        fs::write(t.join(".grove-access"), b"").unwrap();
        set_age(&t.join(".grove-access"), age);
    }

    let mut opts = CleanOpts::from_env();
    opts.dry_run = true;
    opts.lane_ttl = std::time::Duration::MAX; // disable TTL: isolate the ceiling
    opts.cache_max_bytes = Some(96 * 1024);
    opts.protect_lane_keys = vec!["cccccccccccc".to_string()]; // oldest, but live
    let report = host.clean(&opts).unwrap();

    assert!(report.lanes_removed > 0, "ceiling never fired");
    assert!(
        !report.paths.iter().any(|p| p.ends_with("cccccccccccc")),
        "evicted a protected live lane: {:?}",
        report.paths
    );
    // Coldest unprotected lane goes first.
    assert!(
        report.paths[0].ends_with("dddddddddddd"),
        "not coldest-first: {:?}",
        report.paths
    );
}

/// Over budget with only seeds left: drop coldest other-repo host, keep live seed.
#[test]
fn cache_ceiling_evicts_cold_seeds() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let cache = tmp.path().join("cache");
    let _env = EnvGuard::set(&[("GROVE_CACHE", cache.clone())]);
    let host = CacheHost::open(&repo).unwrap();

    // Live seed (protected host).
    let live_seed = host.seed_target();
    fs::create_dir_all(&live_seed).unwrap();
    fs::write(live_seed.join("blob"), vec![b'L'; 32 * 1024]).unwrap();
    fs::write(live_seed.join(".grove-access"), b"").unwrap();

    // Cold foreign host under the same cache root.
    let cold_host = cache
        .join("other-repo-aaaaaaaaaaaa")
        .join("toolchainbbbbbbbb");
    let cold_seed = cold_host.join("seed").join("target");
    fs::create_dir_all(&cold_seed).unwrap();
    fs::write(cold_seed.join("blob"), vec![b'C'; 64 * 1024]).unwrap();
    fs::write(cold_seed.join(".grove-access"), b"").unwrap();
    set_age(&cold_seed.join(".grove-access"), 5000);

    let mut opts = CleanOpts::from_env();
    opts.dry_run = false;
    opts.lane_ttl = std::time::Duration::MAX;
    opts.cache_max_bytes = Some(40 * 1024);
    opts.protect_host = Some(host.host_root().to_path_buf());
    opts.drop_dead_lanes = true;
    let report = host.clean(&opts).unwrap();

    assert!(
        report.seeds_removed > 0,
        "expected cold seed eviction: {report:?}"
    );
    assert!(
        !cold_host.exists(),
        "cold host should be gone: {}",
        cold_host.display()
    );
    assert!(live_seed.join("blob").is_file(), "live seed must stay");
}

/// Still over budget after foreign hosts: evict this host's coldest lanes,
/// sparing only the current checkout's lane.
#[test]
fn cache_ceiling_falls_back_to_own_lanes() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let _env = EnvGuard::set(&[("GROVE_CACHE", tmp.path().join("cache"))]);
    let host = CacheHost::open(&repo).unwrap();

    let lanes = host.host_root().join("lanes");
    for (key, age) in [("currentlane1", 100), ("coldlane2222", 5000)] {
        let t = lanes.join(key).join("target");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("blob"), vec![b'x'; 64 * 1024]).unwrap();
        fs::write(t.join(".grove-access"), b"").unwrap();
        set_age(&t.join(".grove-access"), age);
    }

    let mut opts = CleanOpts::from_env();
    opts.dry_run = true;
    opts.lane_ttl = std::time::Duration::MAX;
    opts.cache_max_bytes = Some(64 * 1024);
    // Both lanes are live worktrees; only the current checkout's lane is sacred.
    opts.protect_lane_keys = vec!["currentlane1".into(), "coldlane2222".into()];
    opts.current_lane_key = Some("currentlane1".into());
    let report = host.clean(&opts).unwrap();

    assert!(
        report.paths.iter().any(|p| p.ends_with("coldlane2222")),
        "cold own lane should be evicted: {report:?}"
    );
    assert!(
        !report.paths.iter().any(|p| p.ends_with("currentlane1")),
        "current lane must never be evicted: {report:?}"
    );
}

/// Legacy `stable-*` source views are reclaimed by any clean pass.
#[test]
fn clean_reclaims_legacy_stable_views() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let cache = tmp.path().join("cache");
    let _env = EnvGuard::set(&[("GROVE_CACHE", cache.clone())]);
    let host = CacheHost::open(&repo).unwrap();

    let stale_view = cache.join("stable-deadbeef1234").join("stable-src");
    fs::create_dir_all(&stale_view).unwrap();
    fs::write(stale_view.join("lib.rs"), "x").unwrap();

    let mut opts = CleanOpts::from_env();
    opts.dry_run = false;
    opts.lane_ttl = std::time::Duration::MAX;
    opts.cache_max_bytes = None;
    let report = host.clean(&opts).unwrap();
    assert!(report.touched(), "{report:?}");
    assert!(!cache.join("stable-deadbeef1234").exists());
}

/// Hygiene drops agent lanes that are not live worktrees without waiting for TTL.
#[test]
fn hygiene_drops_dead_lanes_immediately() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let _env = EnvGuard::set(&[("GROVE_CACHE", tmp.path().join("cache"))]);
    let host = CacheHost::open(&repo).unwrap();

    let dead = host.host_root().join("lanes").join("deaddeaddead");
    fs::create_dir_all(dead.join("target")).unwrap();
    fs::write(dead.join("target/blob"), vec![b'x'; 4096]).unwrap();
    fs::write(dead.join("target/.grove-access"), b"").unwrap(); // fresh

    let report = host.hygiene(&repo).unwrap();
    assert!(report.lanes_removed >= 1, "{report:?}");
    assert!(!dead.exists(), "dead lane should be removed immediately");
}

fn set_old(p: &Path) {
    set_age(p, 60 * 60 * 24 * 365);
}

fn set_age(p: &Path, secs: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
    filetime::set_file_mtime(p, filetime::FileTime::from_system_time(t)).unwrap();
}
