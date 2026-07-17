use super::*;
use crate::api::Grove;
use std::ffi::OsStr;
use tempfile::tempdir;

fn snapshot(grove: &Grove) -> (bool, bool, usize, String, Option<String>, Option<String>) {
    let lane = grove.lane().unwrap();
    let mut command = Command::new("cargo");
    apply_env(&mut command, &lane);
    (
        lane.keep_debuginfo,
        lane.require_cow,
        lane.cpu_slots,
        lane.policy_sha256.clone(),
        command_env(&command, "CARGO_PROFILE_DEV_DEBUG")
            .map(|value| value.to_string_lossy().into_owned()),
        command_env(&command, "CARGO_PROFILE_TEST_DEBUG")
            .map(|value| value.to_string_lossy().into_owned()),
    )
}

fn command_env<'a>(command: &'a Command, key: &str) -> Option<&'a OsStr> {
    command
        .get_envs()
        .find(|(name, _)| *name == OsStr::new(key))
        .and_then(|(_, value)| value)
}

#[test]
fn lane_policy_stays_bound_to_its_workspace() {
    for key in [
        "GROVE_KEEP_DEBUGINFO",
        "GROVE_REQUIRE_COW",
        "GROVE_CPU_SLOTS",
    ] {
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::remove_var(key) };
    }
    let old_cwd = std::env::current_dir().unwrap();
    let base = tempdir().unwrap();
    let (a, b) = (base.path().join("a"), base.path().join("b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(
        a.join(".grove.toml"),
        "keep_debuginfo = true\nrequire_cow = true\ncpu_slots = 2\n",
    )
    .unwrap();
    fs::write(
        b.join(".grove.toml"),
        "keep_debuginfo = false\nrequire_cow = false\ncpu_slots = 3\n",
    )
    .unwrap();

    let root = tempdir().unwrap();
    std::env::set_current_dir(&b).unwrap();
    let a_first = Grove::with_root(root.path().to_path_buf(), &a);
    let b_second = Grove::with_root(root.path().to_path_buf(), &b);
    std::env::set_current_dir(&a).unwrap();
    let b_first = Grove::with_root(root.path().to_path_buf(), &b);
    let a_second = Grove::with_root(root.path().to_path_buf(), &a);
    std::env::set_current_dir(old_cwd).unwrap();

    fs::write(
        a.join(".grove.toml"),
        "keep_debuginfo = false\nrequire_cow = false\ncpu_slots = 9\n",
    )
    .unwrap();
    fs::write(
        b.join(".grove.toml"),
        "keep_debuginfo = true\nrequire_cow = true\ncpu_slots = 8\n",
    )
    .unwrap();

    let a_first_policy = snapshot(&a_first);
    let a_second_policy = snapshot(&a_second);
    let b_first_policy = snapshot(&b_first);
    let b_second_policy = snapshot(&b_second);
    assert_eq!(a_first_policy, a_second_policy);
    assert_eq!(b_first_policy, b_second_policy);
    assert_ne!(a_first_policy.3, b_first_policy.3);
    assert_eq!(
        (a_first_policy.0, a_first_policy.1, a_first_policy.2),
        (true, true, 2),
    );
    assert_eq!(
        (b_first_policy.0, b_first_policy.1, b_first_policy.2),
        (false, false, 3),
    );
    assert_eq!((&a_first_policy.4, &a_first_policy.5), (&None, &None));
    assert_eq!(
        (&b_first_policy.4, &b_first_policy.5),
        (&Some("0".to_string()), &Some("0".to_string())),
    );

    let tagged = a_first.tagged_lane("probe").unwrap();
    assert!(tagged_busy(
        root.path(),
        &a_first.workspace().to_string_lossy(),
        a_first.toolchain(),
        "probe",
    ));
    let meta_path = tagged.dir.join(".grove-meta.json");
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    legacy.as_object_mut().unwrap().remove("tag");
    write_atomic(&meta_path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert!(tagged_busy(
        root.path(),
        &a_first.workspace().to_string_lossy(),
        a_first.toolchain(),
        "probe",
    ));
    drop(tagged);
}
