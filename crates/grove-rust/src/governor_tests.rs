use super::*;
use rustix::fs::{Mode, OFlags};

fn open_pool(path: &Path) -> std::fs::File {
    std::fs::File::from(
        rustix::fs::open(path, OFlags::RDWR | OFlags::NONBLOCK, Mode::empty()).unwrap(),
    )
}

#[test]
fn best_effort_pool_heals_only_after_idle() {
    let root = tempfile::tempdir().unwrap();
    {
        let _first = join_best_effort(root.path(), 4).unwrap();
        assert_eq!(
            drain_strict(&mut open_pool(&root.path().join("jobserver"))).unwrap(),
            3
        );
        let _second = join_best_effort(root.path(), 9).unwrap();
        assert_eq!(
            drain_strict(&mut open_pool(&root.path().join("jobserver"))).unwrap(),
            0
        );
    }
    let _healed = join_best_effort(root.path(), 6).unwrap();
    assert_eq!(
        drain_strict(&mut open_pool(&root.path().join("jobserver"))).unwrap(),
        5
    );
}

#[test]
fn strict_pool_accounts_for_implicit_slots_and_builder_bound() {
    let root = tempfile::tempdir().unwrap();
    let first = join_strict(root.path(), 6, 2, Admission::Wait)
        .unwrap()
        .unwrap();
    let second = join_strict(root.path(), 6, 2, Admission::Wait)
        .unwrap()
        .unwrap();
    assert_eq!(
        drain_strict(&mut open_pool(&root.path().join("jobserver-strict"))).unwrap(),
        4
    );
    let locks = root.path().join("locks");
    assert!(try_admit(&locks, 2).unwrap().is_none());
    drop(first);
    assert!(try_admit(&locks, 2).unwrap().is_some());
    drop(second);
}

#[test]
fn strict_pool_rejects_live_policy_changes_and_invalid_bounds() {
    let root = tempfile::tempdir().unwrap();
    let _held = join_strict(root.path(), 6, 2, Admission::Wait).unwrap();
    assert!(join_strict(root.path(), 5, 2, Admission::Wait).is_err());
    assert!(join_strict(root.path(), 2, 3, Admission::Wait).is_err());
    assert!(join_strict(root.path(), 2, 0, Admission::Wait).is_err());
}

#[test]
fn queued_membership_releases_admission_and_policy_after_use() {
    let root = tempfile::tempdir().unwrap();
    let first = join_strict(root.path(), 2, 1, Admission::Wait)
        .unwrap()
        .unwrap();
    let policy = StrictPolicy {
        schema_version: 1,
        cpu_slots: 2,
        max_builders: 1,
    };
    let (fifo, membership, locks) = strict_membership(root.path(), &policy, Admission::Wait)
        .unwrap()
        .unwrap();
    assert!(try_admit(&locks, 1).unwrap().is_none());

    drop(first);
    let admission = admit(&locks, 1, Admission::Wait).unwrap().unwrap();
    assert!(join_strict(root.path(), 3, 1, Admission::Wait).is_err());
    drop((admission, membership, fifo));

    assert!(join_strict(root.path(), 3, 1, Admission::Wait).is_ok());
}

#[test]
fn strict_pool_rejects_a_non_fifo_boundary() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("jobserver-strict"), b"not a fifo").unwrap();
    assert!(join_strict(root.path(), 2, 1, Admission::Wait).is_err());
}

#[test]
fn try_admission_returns_immediately_when_all_builders_are_held() {
    let root = tempfile::tempdir().unwrap();
    let _held = join_strict(root.path(), 2, 1, Admission::Wait)
        .unwrap()
        .unwrap();

    assert!(
        join_strict(root.path(), 2, 1, Admission::Try)
            .unwrap()
            .is_none()
    );
}

#[test]
fn strict_drain_retries_interrupts_and_propagates_other_errors() {
    let mut step = 0;
    let drained = drain_with(|_| {
        step += 1;
        match step {
            1 => Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
            2 => Ok(2),
            _ => Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
        }
    })
    .unwrap();
    assert_eq!(drained, 2);

    let error = drain_with(|_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn flags_use_inherited_descriptors_not_a_fifo_path() {
    let root = tempfile::tempdir().unwrap();
    let pool = Pool::join(root.path(), 4).unwrap();
    let flags = pool.flags().unwrap();
    assert!(flags.contains("--jobserver-auth="));
    assert!(!flags.contains("fifo:"));
}

#[test]
fn command_configuration_overwrites_every_jobserver_variable() {
    let root = tempfile::tempdir().unwrap();
    let pool = Pool::join(root.path(), 4).unwrap();
    let expected = pool.flags().unwrap();
    let mut command = std::process::Command::new("unused");
    for name in ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"] {
        command.env(name, "stale");
    }

    pool.configure(&mut command);

    for name in ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"] {
        assert_eq!(
            command.get_envs().find(|(key, _)| *key == name).unwrap().1,
            Some(expected.as_ref())
        );
    }
}
