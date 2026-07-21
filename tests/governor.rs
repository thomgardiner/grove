use grove::cache;
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
use tempfile::tempdir;

fn workspace(
    base: &Path,
    name: &str,
    mode: &str,
    slots: usize,
    builders: usize,
) -> std::path::PathBuf {
    let workspace = base.join(name);
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        workspace.join(".grove.toml"),
        format!("governor_mode = \"{mode}\"\ncpu_slots = {slots}\nmax_builders = {builders}\n"),
    )
    .unwrap();
    workspace
}

#[cfg(unix)]
fn pool_tokens(path: &Path) -> usize {
    use rustix::fs::{Mode, OFlags};
    use std::io::Read;

    let mut fifo = std::fs::File::from(
        rustix::fs::open(path, OFlags::RDWR | OFlags::NONBLOCK, Mode::empty()).unwrap(),
    );
    let mut total = 0;
    let mut buf = [0; 64];
    loop {
        match fifo.read(&mut buf) {
            Ok(0) => return total,
            Ok(read) => total += read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return total,
            Err(error) => panic!("reading strict jobserver tokens: {error}"),
        }
    }
}

#[cfg(unix)]
#[test]
fn strict_configuration_reserves_each_builders_implicit_slot() {
    // SAFETY: nextest runs each test in its own process.
    unsafe {
        std::env::remove_var("GROVE_GOVERNOR_MODE");
        std::env::remove_var("GROVE_CPU_SLOTS");
        std::env::remove_var("GROVE_MAX_BUILDERS");
    }
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let a = workspace(base.path(), "a", "strict", 6, 2);
    let b = workspace(base.path(), "b", "strict", 6, 2);

    let first = cache::acquire(&root, &a.to_string_lossy(), "stable").unwrap();
    let _second = cache::acquire(&root, &b.to_string_lossy(), "stable").unwrap();
    let mut command = Command::new("cargo");
    cache::apply_env(&mut command, &first);
    let jobserver = |name| {
        command
            .get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value)
            .expect("strict lane exports its held jobserver")
            .to_string_lossy()
            .into_owned()
    };

    assert_eq!(pool_tokens(&root.join("jobserver-strict")), 4);
    for name in ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"] {
        assert!(jobserver(name).contains("jobserver-strict"));
    }
}

#[cfg(unix)]
#[test]
fn strict_descendant_keeps_admission_and_lane_locks_after_grove_drops_them() {
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let a = workspace(base.path(), "a", "strict", 2, 1);
    let b = workspace(base.path(), "b", "strict", 2, 1);
    let lane = cache::acquire(&root, &a.to_string_lossy(), "stable").unwrap();
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 2 &"]);
    cache::apply_env(&mut command, &lane);

    assert!(command.status().unwrap().success());
    drop(lane);

    assert!(cache::workspace_busy(&root, &a.to_string_lossy(), None));
    assert!(
        cache::try_acquire(&root, &b.to_string_lossy(), "stable")
            .unwrap()
            .is_none()
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while cache::workspace_busy(&root, &a.to_string_lossy(), None) {
        assert!(
            std::time::Instant::now() < deadline,
            "descendant did not release inherited locks"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(
        cache::try_acquire(&root, &b.to_string_lossy(), "stable")
            .unwrap()
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn strict_configuration_refuses_an_unenforceable_fifo() {
    let base = tempdir().unwrap();
    let root = base.path().join("cache");
    let workspace = workspace(base.path(), "repo", "strict", 2, 1);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("jobserver-strict"), b"not a fifo").unwrap();

    let error = cache::acquire(&root, &workspace.to_string_lossy(), "stable")
        .err()
        .expect("strict acquisition refuses a regular file");

    assert!(
        error
            .to_string()
            .contains("strict build governor unavailable")
    );
}

#[test]
fn invalid_governor_environment_refuses_lane_acquisition() {
    let base = tempdir().unwrap();
    let workspace = workspace(base.path(), "repo", "best_effort", 2, 1);
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("GROVE_GOVERNOR_MODE", "almost-strict") };

    let error = cache::acquire(base.path(), &workspace.to_string_lossy(), "stable")
        .err()
        .expect("invalid mode refuses a build lane");
    unsafe { std::env::remove_var("GROVE_GOVERNOR_MODE") };

    assert!(
        error
            .to_string()
            .contains("invalid build governor configuration")
    );
}

#[test]
fn strict_invalid_numeric_limits_refuse_lane_acquisition() {
    let base = tempdir().unwrap();
    let repo = workspace(base.path(), "repo", "strict", 2, 1);
    // SAFETY: nextest runs each test in its own process.
    unsafe {
        std::env::set_var("GROVE_GOVERNOR_MODE", "strict");
        std::env::set_var("GROVE_CPU_SLOTS", "garbage");
    }
    let cpu_error = cache::acquire(base.path(), &repo.to_string_lossy(), "stable")
        .err()
        .expect("malformed strict CPU limit refuses a lane");
    unsafe {
        std::env::remove_var("GROVE_CPU_SLOTS");
        std::env::set_var("GROVE_MAX_BUILDERS", "garbage");
    }
    let builder_error = cache::acquire(base.path(), &repo.to_string_lossy(), "stable")
        .err()
        .expect("malformed strict builder limit refuses a lane");
    unsafe {
        std::env::remove_var("GROVE_GOVERNOR_MODE");
        std::env::remove_var("GROVE_MAX_BUILDERS");
    }

    assert!(
        cpu_error
            .to_string()
            .contains("invalid build governor configuration")
    );
    assert!(
        builder_error
            .to_string()
            .contains("invalid build governor configuration")
    );
    let zero = workspace(base.path(), "zero", "strict", 0, 1);
    assert!(cache::acquire(base.path(), &zero.to_string_lossy(), "stable").is_err());
}

#[cfg(not(unix))]
#[test]
fn strict_configuration_is_rejected_on_unsupported_platforms() {
    let base = tempdir().unwrap();
    let workspace = workspace(base.path(), "repo", "strict", 2, 1);

    let error = cache::acquire(base.path(), &workspace.to_string_lossy(), "stable")
        .err()
        .expect("strict acquisition is unsupported");

    assert!(error.to_string().contains("supported only on Unix"));
}
