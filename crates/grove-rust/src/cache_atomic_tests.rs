use super::write_atomic;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn creates_nested_parent_and_replaces_complete_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("records/nested/state.json");
    write_atomic(&path, b"a much longer first record").unwrap();
    write_atomic(&path, b"short").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"short");
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn reader_observes_old_value_until_synced_temp_is_published() {
    use super::{PROCESS_NONCE, PROCESS_START, create_temp, publish_after};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state.json");
    let old = vec![b'a'; 1024];
    let new = vec![b'b'; 8192];
    write_atomic(&path, &old).unwrap();
    let sequence = AtomicU64::new(0);
    let (temp, file) = create_temp(
        root.path(),
        std::process::id(),
        *PROCESS_START,
        *PROCESS_NONCE,
        &sequence,
    )
    .unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let writer_path = path.clone();
        let writer_new = new.clone();
        let writer = scope.spawn(move || {
            publish_after(&temp, &writer_path, &writer_new, file, || {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let observed = fs::read(&path);
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        assert_eq!(observed.unwrap(), old);
    });
    assert_eq!(fs::read(path).unwrap(), new);
}

#[test]
fn dead_temps_are_swept_but_live_temps_are_preserved() {
    use super::{PROCESS_NONCE, PROCESS_START};

    let root = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    let started = (*PROCESS_START)
        .map(|value| format!("{value:016x}"))
        .unwrap_or_else(|| "unknown".to_owned());
    let nonce = *PROCESS_NONCE;
    assert!(nonce > u64::MAX as u128);
    let dead = root
        .path()
        .join(".grove-record-4294967295-0000000000000000-00000000000000000000000000000000-0.tmp");
    let prior_incarnation = root.path().join(format!(
        ".grove-record-{pid}-{started}-{:032x}-0.tmp",
        nonce.wrapping_add(1)
    ));
    let live = root
        .path()
        .join(format!(".grove-record-{pid}-{started}-{nonce:032x}-99.tmp"));
    fs::write(&dead, b"dead").unwrap();
    fs::write(&prior_incarnation, b"stale").unwrap();
    fs::write(&live, b"live").unwrap();
    write_atomic(&root.path().join("state.json"), b"state").unwrap();
    assert!(!dead.exists());
    assert!(!prior_incarnation.exists());
    assert_eq!(fs::read(live).unwrap(), b"live");
}

#[test]
fn reused_live_pid_with_different_start_is_swept() {
    use sysinfo::System;

    let current = std::process::id();
    let system = System::new_all();
    let (pid, process) = system
        .processes()
        .iter()
        .filter(|(pid, _)| pid.as_u32() != current)
        .min_by_key(|(pid, _)| pid.as_u32())
        .unwrap();
    let pid = pid.as_u32();
    let stale_start = process.start_time().wrapping_add(1);
    let root = tempfile::tempdir().unwrap();
    let stale = root.path().join(format!(
        ".grove-record-{pid}-{stale_start:016x}-00000000000000000000000000000001-0.tmp"
    ));
    fs::write(&stale, b"stale").unwrap();
    write_atomic(&root.path().join("state"), b"ok").unwrap();
    assert!(!stale.exists());
}

#[test]
fn process_liveness_distinguishes_current_and_missing_pid() {
    use super::pid_alive;

    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(u32::MAX));
}

#[test]
fn failed_replacement_removes_its_temp_file() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("directory");
    fs::create_dir(&destination).unwrap();
    assert!(write_atomic(&destination, b"state").is_err());
    let names: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0], destination.file_name().unwrap());
}

#[test]
fn temp_name_collisions_retry_without_deleting_the_collision() {
    use super::{create_temp, publish};

    let root = tempfile::tempdir().unwrap();
    let nonce = 11;
    let collision = root.path().join(format!(
        ".grove-record-7-0000000000000003-{nonce:032x}-0.tmp"
    ));
    fs::write(&collision, b"live").unwrap();
    let sequence = AtomicU64::new(0);
    let (temp, file) = create_temp(root.path(), 7, Some(3), nonce, &sequence).unwrap();
    assert_ne!(temp, collision);
    publish(&temp, &root.path().join("state"), b"ok", file).unwrap();
    assert_eq!(fs::read(collision).unwrap(), b"live");
    assert_eq!(fs::read(root.path().join("state")).unwrap(), b"ok");
}

#[test]
fn temp_creation_reports_non_collision_errors() {
    use super::create_temp;

    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("not-a-directory");
    fs::write(&parent, b"file").unwrap();
    let error = create_temp(&parent, 7, Some(3), 11, &AtomicU64::new(0)).unwrap_err();
    assert!(format!("{error:#}").contains("creating temp file"));
}

#[test]
fn relative_paths_start_at_the_current_directory() {
    let root = tempfile::Builder::new()
        .prefix(".grove-atomic-relative-")
        .tempdir_in(".")
        .unwrap();
    let relative = PathBuf::from(root.path().file_name().unwrap());
    assert!(!relative.is_absolute());
    let path = relative.join("cache/nested/state");
    write_atomic(&path, b"ok").unwrap();
    assert_eq!(fs::read(path).unwrap(), b"ok");
}

#[cfg(unix)]
#[test]
fn directory_sync_chain_includes_existing_ancestor() {
    use super::created_chain;
    use std::path::Path;

    let parent = Path::new("cache/acquisitions/repo");
    let existing = Path::new("cache");
    assert_eq!(
        created_chain(parent, existing),
        [parent, Path::new("cache/acquisitions"), existing]
    );
}

#[cfg(unix)]
#[test]
fn directory_sync_failures_are_reported() {
    use super::{sync_created, sync_parent};

    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    assert!(sync_created(&missing, root.path()).is_err());
    assert!(sync_parent(&missing).is_err());
}

#[cfg(windows)]
#[test]
fn windows_replaces_beyond_legacy_max_path() {
    use std::os::windows::ffi::OsStrExt;

    let root = tempfile::tempdir().unwrap();
    let mut directory = root.path().to_path_buf();
    while directory.as_os_str().encode_wide().count() < 280 {
        directory.push("long-segment");
    }
    let path = directory.join("state.json");
    write_atomic(&path, b"first").unwrap();
    write_atomic(&path, b"second").unwrap();
    assert_eq!(fs::read(path).unwrap(), b"second");
}

#[cfg(windows)]
#[test]
fn windows_rejects_interior_nul_without_touching_prefix() {
    use std::ffi::{OsStr, OsString};
    use std::io::ErrorKind;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let root = tempfile::tempdir().unwrap();
    let victim = root.path().join("victim");
    fs::write(&victim, b"safe").unwrap();
    let mut name: Vec<_> = OsStr::new("victim").encode_wide().collect();
    name.extend([0, b'x' as u16]);
    let error = write_atomic(&root.path().join(OsString::from_wide(&name)), b"bad").unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(victim).unwrap(), b"safe");
}
