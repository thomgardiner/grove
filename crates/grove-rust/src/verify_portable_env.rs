//! Controlled child environment and lane arguments for portable verification.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::process::Command;

use crate::cache;

use super::{CommandKind, command_kind};

pub(super) fn effective_lane_environment(
    values: &mut BTreeMap<OsString, OsString>,
    keep_debuginfo: bool,
) {
    if keep_debuginfo {
        return;
    }
    values.insert("CARGO_PROFILE_DEV_DEBUG".into(), "0".into());
    values.insert("CARGO_PROFILE_TEST_DEBUG".into(), "0".into());
    if cfg!(target_os = "macos") {
        values.insert("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO".into(), "off".into());
        values.insert("CARGO_PROFILE_TEST_SPLIT_DEBUGINFO".into(), "off".into());
    }
}

/// The exact child environment whose digest a portable receipt records.
pub(super) fn child(names: &[String], keep_debuginfo: bool) -> BTreeMap<OsString, OsString> {
    let mut values = env::vars_os()
        .filter(|(name, _)| allowed(name, names) && !jobserver(name))
        .map(|(name, value)| (canonical(name, cfg!(windows)), value))
        .collect::<BTreeMap<_, _>>();
    values.insert("CARGO_TARGET_DIR".into(), ".grove-target".into());
    values.remove(OsStr::new("CARGO_BUILD_BUILD_DIR"));
    effective_lane_environment(&mut values, keep_debuginfo);
    values
}

/// Apply the same controlled environment whose digest is stored in a portable receipt.
/// Target paths are stable text for child processes; Cargo receives the actual isolated
/// lane through its native `--target-dir` argument instead.
pub(super) fn configure_command(command: &mut Command, names: &[String], keep_debuginfo: bool) {
    command.env_clear().envs(child(names, keep_debuginfo));
}

/// The actual lane remains isolated even though the child-visible target environment
/// is stable between clean checkouts. This is only called for `CommandKind::Builds`.
pub(super) fn command_args(argv: &[String], lane: &cache::Lane) -> Vec<String> {
    let mut args = argv[1..].to_vec();
    if command_kind(argv) == Some(CommandKind::Builds) {
        let index = args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(args.len());
        args.splice(
            index..index,
            [
                "--target-dir".to_string(),
                lane.target_dir.to_string_lossy().into_owned(),
            ],
        );
    }
    args
}

fn allowed(name: &OsStr, names: &[String]) -> bool {
    allowed_on(name, names, cfg!(windows))
}

fn jobserver(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
    })
}

fn allowed_on(name: &OsStr, names: &[String], case_insensitive: bool) -> bool {
    let name = name.to_string_lossy();
    names
        .iter()
        .any(|declared| same(&name, declared, case_insensitive))
        || [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SYSTEMROOT",
            "WINDIR",
            "ComSpec",
            "TEMP",
            "TMP",
            "TMPDIR",
            "SDKROOT",
            "DEVELOPER_DIR",
            "MACOSX_DEPLOYMENT_TARGET",
            "RUSTC",
            "RUSTDOC",
            "RUSTFLAGS",
            "RUST_BACKTRACE",
            "RUST_LOG",
            "RUST_TEST_THREADS",
            "RUST_MIN_STACK",
            "RUSTC_BOOTSTRAP",
            "CC",
            "CXX",
            "AR",
            "RANLIB",
            "CFLAGS",
            "CXXFLAGS",
            "CPPFLAGS",
            "LDFLAGS",
            "LIBRARY_PATH",
            "CPATH",
            "INCLUDE",
            "LIB",
            "LIBPATH",
        ]
        .iter()
        .any(|fixed| same(&name, fixed, case_insensitive))
        || [
            "CARGO_",
            "RUSTUP_",
            "RUSTC_",
            "CC_",
            "CXX_",
            "AR_",
            "RANLIB_",
            "CFLAGS_",
            "CXXFLAGS_",
            "PKG_CONFIG_",
            "OPENSSL_",
        ]
        .iter()
        .any(|prefix| prefixed(&name, prefix, case_insensitive))
}

fn same(left: &str, right: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn prefixed(name: &str, prefix: &str, case_insensitive: bool) -> bool {
    name.get(..prefix.len())
        .is_some_and(|start| same(start, prefix, case_insensitive))
}

fn canonical(name: OsString, case_insensitive: bool) -> OsString {
    if case_insensitive {
        name.to_string_lossy().to_ascii_uppercase().into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_name_matching_is_ascii_case_insensitive() {
        assert!(allowed_on(OsStr::new("Path"), &[], true));
        assert!(allowed_on(OsStr::new("cargo_home"), &[], true));
        assert!(allowed_on(
            OsStr::new("nexus_release_mode"),
            &["NEXUS_RELEASE_MODE".into()],
            true,
        ));
    }

    #[test]
    fn windows_receipt_keys_have_one_canonical_case() {
        assert_eq!(canonical(OsString::from("Path"), true), "PATH");
        assert_eq!(canonical(OsString::from("cargo_home"), true), "CARGO_HOME");
    }

    #[test]
    fn controlled_environment_excludes_ephemeral_jobserver_descriptors() {
        let values = child(&[], false);
        for name in ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"] {
            assert!(!values.contains_key(OsStr::new(name)));
        }
    }
}
