//! Controlled child environment and lane arguments for portable verification.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::process::Command;

use crate::cache;

use super::{CommandKind, command_kind, effective_lane_environment};

/// The exact child environment whose digest a portable receipt records.
pub(super) fn child(names: &[String]) -> BTreeMap<OsString, OsString> {
    let mut values = env::vars_os()
        .filter(|(name, _)| allowed(name, names))
        .collect::<BTreeMap<_, _>>();
    values.insert("CARGO_TARGET_DIR".into(), ".grove-target".into());
    values.remove(OsStr::new("CARGO_BUILD_BUILD_DIR"));
    effective_lane_environment(&mut values);
    values
}

/// Apply the same controlled environment whose digest is stored in a portable receipt.
/// Target paths are stable text for child processes; Cargo receives the actual isolated
/// lane through its native `--target-dir` argument instead.
pub(super) fn configure_command(command: &mut Command, names: &[String]) {
    command.env_clear().envs(child(names));
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
    let name = name.to_string_lossy();
    names.iter().any(|declared| declared == &name)
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
        .contains(&name.as_ref())
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
        .any(|prefix| name.starts_with(prefix))
}
