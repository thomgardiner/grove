//! A copy-on-write seed clones a warm lane's target at a NEW path, but Cargo bakes
//! each build script's absolute OUT_DIR into its run output — a path that points back
//! at the source lane. Seeding must purge that run output (and its run fingerprint) so
//! the script reruns in the new lane, while keeping the compiled build-script binaries
//! and crate rlibs the clone carries. This is the regression test for a Tauri build
//! failing in a seeded lane because it read a permission file at the source lane's path.

use grove::cache;
use std::fs;
use tempfile::tempdir;

#[test]
fn seed_purges_build_script_run_output_but_keeps_binaries_and_rlibs() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let ws_str = ws.path().to_string_lossy().into_owned();

    let canonical = cache::canonical_dir(root.path(), &ws_str, "stable");
    let cbuild = canonical.join("build/debug");

    // A build script's RUN output — the dir holding `output`, carrying a stale
    // absolute OUT_DIR that would point back at the source lane.
    fs::create_dir_all(cbuild.join("build/demo-run/out")).unwrap();
    fs::write(
        cbuild.join("build/demo-run/output"),
        b"cargo:out_dir=/old/source/lane/build/demo-run/out",
    )
    .unwrap();
    fs::create_dir_all(cbuild.join(".fingerprint/demo-run")).unwrap();
    fs::write(
        cbuild.join(".fingerprint/demo-run/run-build-script-abc"),
        b"fp",
    )
    .unwrap();

    // The compiled build-script BINARY (no `output` file) — must survive.
    fs::create_dir_all(cbuild.join("build/demo-bin")).unwrap();
    fs::write(cbuild.join("build/demo-bin/build-script-build"), b"bin").unwrap();

    // A compiled dependency rlib — must survive; keeping it is the whole point of
    // seeding copy-on-write instead of cold-building.
    fs::create_dir_all(cbuild.join("deps")).unwrap();
    fs::write(cbuild.join("deps/libdemo.rlib"), b"rlib").unwrap();

    let lane = cache::acquire(root.path(), &ws_str, "stable").unwrap();
    assert!(
        cache::seed(root.path(), &lane, &canonical).unwrap(),
        "a cold lane with a canonical seeds"
    );

    let lbuild = lane.build_dir.join("debug");
    assert!(
        !lbuild.join("build/demo-run").exists(),
        "the build-script run output (stale OUT_DIR) must be purged"
    );
    assert!(
        !lbuild
            .join(".fingerprint/demo-run/run-build-script-abc")
            .exists(),
        "the run fingerprint must be purged so Cargo reruns the script"
    );
    assert!(
        lbuild.join("build/demo-bin/build-script-build").exists(),
        "the compiled build-script binary must survive the seed"
    );
    assert!(
        lbuild.join("deps/libdemo.rlib").exists(),
        "compiled rlibs must survive the seed (the copy-on-write win)"
    );
}
