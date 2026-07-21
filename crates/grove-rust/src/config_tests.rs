use super::*;

#[test]
fn per_repo_config_overrides_global_and_keeps_unset_globals() {
    let mut base = Config {
        cache_root: Some("/global/cache".into()),
        min_free_gb: Some(20),
        keep_debuginfo: Some(true),
        ..Config::default()
    };
    let over = Config {
        min_free_gb: Some(50),
        max_canonical_gb: Some(40),
        require_cow: Some(true),
        ..Config::default()
    };
    merge(&mut base, over);

    assert_eq!(base.min_free_gb, Some(50));
    assert_eq!(base.max_canonical_gb, Some(40));
    assert_eq!(base.require_cow, Some(true));
    assert_eq!(base.cache_root.as_deref(), Some("/global/cache"));
    assert_eq!(base.keep_debuginfo, Some(true));
}

#[test]
fn repo_config_is_found_from_a_subdirectory() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join(".grove.toml"), "min_free_gb = 7\n").unwrap();
    let deep = repo.path().join("src").join("nested");
    std::fs::create_dir_all(&deep).unwrap();

    let found = Config::repository(&deep).expect("ancestor walk finds the repo config");

    assert_eq!(
        crate::cache::canonical_path(&found),
        crate::cache::canonical_path(&repo.path().join(".grove.toml"))
    );
    assert_eq!(read(&found).unwrap().min_free_gb, Some(7));
}

#[test]
fn unparseable_config_is_retained_as_an_invalid_safety_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".grove.toml");
    std::fs::write(&path, "min_free_gb = 7\nkeep_debug = true\n").unwrap();

    assert_eq!(read(&path).unwrap().governor(), GovernorMode::Invalid);
}

#[test]
fn home_resolution_uses_each_platforms_native_variable_first() {
    let home = Some(OsString::from("unix-home"));
    let profile = Some(OsString::from("windows-home"));
    assert_eq!(
        home_dir_for(false, home.clone(), profile.clone()),
        Some(PathBuf::from("unix-home"))
    );
    assert_eq!(
        home_dir_for(true, home, profile),
        Some(PathBuf::from("windows-home"))
    );
}

#[test]
fn unreadable_config_is_not_treated_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_text(dir.path()).is_err());
    assert_eq!(read(dir.path()).unwrap().governor(), GovernorMode::Invalid);
}

#[test]
fn missing_config_remains_optional() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read(&dir.path().join("missing.toml")).is_none());
}

#[test]
fn invalid_governor_environment_is_not_silently_defaulted() {
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("GROVE_GOVERNOR_MODE", "almost-strict") };
    assert_eq!(governor::validated(None, None, None), GovernorMode::Invalid);
    unsafe { std::env::remove_var("GROVE_GOVERNOR_MODE") };
}

#[cfg(unix)]
#[test]
fn non_unicode_governor_environment_is_invalid() {
    use std::os::unix::ffi::OsStringExt;

    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("GROVE_GOVERNOR_MODE", OsString::from_vec(vec![b's', 0xff])) };
    assert_eq!(Config::default().governor(), GovernorMode::Invalid);
    unsafe { std::env::remove_var("GROVE_GOVERNOR_MODE") };
}

#[test]
fn malformed_strict_config_refuses_instead_of_defaulting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".grove.toml"),
        "governor_mode = 'strict'\ncpu_slots = 'garbage'\n",
    )
    .unwrap();

    assert_eq!(
        Config::resolve(dir.path()).governor(),
        GovernorMode::Invalid
    );
}

#[test]
fn invalid_governor_config_is_retained_for_fail_closed_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".grove.toml");
    std::fs::write(&path, "governor_mode = 'almost-strict'\n").unwrap();

    assert_eq!(read(&path).unwrap().governor(), GovernorMode::Invalid);
}

#[test]
fn strict_zero_limits_are_invalid_in_repository_config() {
    for source in [
        "governor_mode = 'strict'\ncpu_slots = 0\n",
        "governor_mode = 'strict'\nmax_builders = 0\n",
    ] {
        let config: Config = toml::from_str(source).unwrap();
        assert_eq!(config.governor(), GovernorMode::Invalid);
    }
}

#[test]
fn strict_malformed_environment_limits_are_invalid() {
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("GROVE_GOVERNOR_MODE", "strict") };
    for value in ["garbage", "0"] {
        unsafe { std::env::set_var("GROVE_CPU_SLOTS", value) };
        assert_eq!(Config::default().governor(), GovernorMode::Invalid);
    }
    unsafe {
        std::env::remove_var("GROVE_CPU_SLOTS");
        std::env::set_var("GROVE_MAX_BUILDERS", "garbage");
    }
    assert_eq!(Config::default().governor(), GovernorMode::Invalid);
    unsafe {
        std::env::remove_var("GROVE_GOVERNOR_MODE");
        std::env::remove_var("GROVE_MAX_BUILDERS");
    }
}
