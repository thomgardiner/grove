//! Canonical cache locking, seed repair, and lane promotion.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use super::{Lane, now_secs, write_atomic};

pub(crate) enum Seed {
    Unpublished,
    Warm,
    Cloned,
}

/// A lock guarding one canonical against seed/promote races: seeds take it shared (many
/// lanes clone at once), a promote takes it exclusive (rewrites it alone), so no seed
/// ever reads a canonical mid-rewrite.
pub(super) fn canonical_lock(root: &Path, canonical: &Path) -> Result<File> {
    let name = canonical
        .file_name()
        .context("canonical path has no name")?
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(root.join("locks"))?;
    File::create(root.join("locks").join(format!("canonical-{name}.lock")))
        .context("opening canonical lock")
}

/// A seeded lane is a copy-on-write clone of the canonical at a NEW path, but Cargo
/// bakes each build script's absolute `OUT_DIR` into its run output (`output`,
/// `root-output`, the `out/` tree). Left as-is, a dependent that reads a build
/// script's generated files — Tauri's permission manifests, say — follows the path
/// back into the *source* lane and fails to build. Deleting every run output is not
/// an option either: a missing run fingerprint marks the script's dependents dirty
/// at plan time (`UnitDependencyInfoChanged`), which recompiles essentially the
/// whole graph and erases the seeding win. So reset only the units whose recorded
/// directives actually reference the lane they ran in; pure-directive scripts (cfg
/// probes like libc, serde, anyhow) keep their fingerprints and their dependents
/// stay warm.
fn reset_seeded_build_scripts(dir: &Path) -> Result<()> {
    for base in [dir.join("build"), dir.join("target")] {
        let profiles = match fs::read_dir(&base) {
            Ok(profiles) => profiles,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("reading {}", base.display())),
        };
        for profile in profiles {
            let profile = profile.with_context(|| format!("reading {}", base.display()))?;
            if !profile
                .file_type()
                .with_context(|| format!("inspecting {}", profile.path().display()))?
                .is_dir()
            {
                continue;
            }
            let profile = profile.path();
            // A build script's run output is the `build/<pkg>/` dir holding an
            // `output` file; its sibling holds the compiled binary, which is kept.
            let units = match fs::read_dir(profile.join("build")) {
                Ok(units) => Some(units),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("reading build scripts in {}", profile.display())
                    });
                }
            };
            if let Some(units) = units {
                for unit in units {
                    let unit = unit.with_context(|| {
                        format!("reading build scripts in {}", profile.display())
                    })?;
                    let output = unit.path().join("output");
                    match fs::symlink_metadata(&output) {
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!(
                                    "inspecting seeded build script output {}",
                                    output.display()
                                )
                            });
                        }
                    }
                    if !leaks_source_lane_paths(&unit.path())? {
                        continue;
                    }
                    fs::remove_dir_all(unit.path()).with_context(|| {
                        format!(
                            "removing seeded build script output {}",
                            unit.path().display()
                        )
                    })?;
                    // Drop the matching run fingerprints (the fingerprint dir shares
                    // the unit dir's name) so Cargo reruns this script here.
                    let prints = profile.join(".fingerprint").join(unit.file_name());
                    let files = match fs::read_dir(&prints) {
                        Ok(files) => files,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("reading fingerprint {}", prints.display())
                            });
                        }
                    };
                    for file in files {
                        let file = file
                            .with_context(|| format!("reading fingerprint {}", prints.display()))?;
                        if file
                            .file_name()
                            .to_string_lossy()
                            .starts_with("run-build-script-")
                        {
                            fs::remove_file(file.path()).with_context(|| {
                                format!(
                                    "removing seeded build-script fingerprint {}",
                                    file.path().display()
                                )
                            })?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Whether this build script's recorded directives reference the lane the script ran
/// in. `root-output` records that lane's absolute `OUT_DIR`
/// (`<lane>/build/<profile>/build/<unit>/out`); any directive mentioning that lane —
/// a link-search into `out/`, a metadata path, a `rustc-env` path — would leak a
/// foreign lane's path into this lane's builds. Units without a readable record are
/// reset conservatively.
fn leaks_source_lane_paths(unit: &Path) -> Result<bool> {
    let recorded = match fs::read(unit.join("root-output")) {
        Ok(recorded) => recorded,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading {}", unit.join("root-output").display()));
        }
    };
    let recorded = String::from_utf8_lossy(&recorded);
    let out_dir = Path::new(recorded.trim());
    let Some(lane) = out_dir.ancestors().nth(5) else {
        return Ok(true);
    };
    if lane.as_os_str().is_empty() {
        return Ok(true);
    }
    let output = fs::read(unit.join("output"))
        .with_context(|| format!("reading {}", unit.join("output").display()))?;
    Ok(String::from_utf8_lossy(&output).contains(lane.to_string_lossy().as_ref()))
}

/// Seed a cold lane from its canonical (copy-on-write). A lane that already holds a
/// `target/` is warm and is left untouched. Holds the canonical's lock shared, so it
/// never clones a canonical a concurrent promote is rewriting.
pub fn seed(root: &Path, lane: &Lane, canonical: &Path) -> Result<bool> {
    if lane.target_dir.exists() || !canonical.exists() {
        return Ok(false);
    }
    let lock = canonical_lock(root, canonical)?;
    lock.lock_shared()
        .context("shared-locking canonical for seed")?;
    if !canonical.exists() {
        return Ok(false); // a promote removed it between the check and the lock
    }
    clone(lane, canonical)?;
    touch_canonical(root, canonical);
    Ok(true)
}

pub(crate) fn seed_published(root: &Path, lane: &Lane, canonical: &Path) -> Result<Seed> {
    let lock = canonical_lock(root, canonical)?;
    lock.lock_shared()
        .context("shared-locking canonical for seed")?;
    if !super::retention::published_locked_for_policy(root, canonical, &lane.policy_sha256) {
        return Ok(Seed::Unpublished);
    }
    if lane.target_dir.exists() {
        return Ok(Seed::Warm);
    }
    clone(lane, canonical)?;
    touch_canonical(root, canonical);
    Ok(Seed::Cloned)
}

fn clone(lane: &Lane, canonical: &Path) -> Result<()> {
    // Clone canonical into the lane, then restore the lane's own metadata.
    let meta = fs::read(lane.dir.join(".grove-meta.json")).ok();
    crate::seed::clone_tree_cow_with(canonical, &lane.dir, lane.require_cow, |staging| {
        reset_seeded_build_scripts(staging)?;
        if let Some(meta) = &meta {
            write_atomic(&staging.join(".grove-meta.json"), meta)?;
        }
        Ok(())
    })
}

/// Publish a warmed lane as the canonical. Holds the canonical's lock exclusive, so
/// only one promote runs at a time and no seed reads it mid-rewrite.
pub fn promote(root: &Path, lane: &Lane, canonical: &Path) -> Result<()> {
    let lock = canonical_lock(root, canonical)?;
    lock.lock_exclusive()
        .context("exclusive-locking canonical for promote")?;
    super::retention::unpublish(root, canonical)?;
    crate::seed::clone_tree(&lane.dir, canonical)?;
    super::retention::publish(root, lane, canonical)?;
    touch_canonical(root, canonical);
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub(super) struct CanonicalMeta {
    pub(super) last_used: u64,
}

/// Canonical last-used lives outside the canonical dir, so touching it never mutates a
/// canonical while lanes are cloning it.
pub(super) fn canonical_meta_path(root: &Path, canonical: &Path) -> PathBuf {
    let name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    root.join("canonical-meta").join(format!("{name}.json"))
}

/// Mark a canonical recently used (on every seed and promote), so GC evicts the coldest
/// canonicals first.
fn touch_canonical(root: &Path, canonical: &Path) {
    if let Ok(bytes) = serde_json::to_vec(&CanonicalMeta {
        last_used: now_secs(),
    }) {
        let _ = write_atomic(&canonical_meta_path(root, canonical), &bytes);
    }
}

pub(super) fn canonical_last_used(root: &Path, canonical: &Path) -> u64 {
    fs::read(canonical_meta_path(root, canonical))
        .ok()
        .and_then(|b| serde_json::from_slice::<CanonicalMeta>(&b).ok())
        .map(|m| m.last_used)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Grove;
    use tempfile::tempdir;

    #[test]
    fn seeding_repairs_build_script_outputs_before_publishing_the_lane() {
        let root = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
        let source = grove.tagged_lane("source").unwrap();
        let unit = source.target_dir.join("debug/build/example");
        let fingerprint = source
            .target_dir
            .join("debug/.fingerprint/example/run-build-script-example");
        fs::create_dir_all(&unit).unwrap();
        fs::write(unit.join("output"), "OUT_DIR=/old/lane").unwrap();
        fs::create_dir_all(fingerprint.parent().unwrap()).unwrap();
        fs::write(&fingerprint, "old run output").unwrap();
        grove.promote(&source).unwrap();
        drop(source);

        let seeded = grove.seeded_tagged_lane("consumer").unwrap();

        assert!(!seeded.target_dir.join("debug/build/example").exists());
        assert!(
            !seeded
                .target_dir
                .join("debug/.fingerprint/example/run-build-script-example")
                .exists()
        );
    }

    #[test]
    fn seeding_resets_only_build_scripts_that_leak_their_lane() {
        let root = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
        let source = grove.tagged_lane("source").unwrap();
        for (name, output) in [
            ("probe", "cargo:rustc-cfg=freebsd12\n".to_string()),
            (
                "leaky",
                format!(
                    "cargo:rustc-link-search={}\n",
                    source.target_dir.join("debug/build/leaky/out").display()
                ),
            ),
        ] {
            let unit = source.target_dir.join("debug/build").join(name);
            fs::create_dir_all(&unit).unwrap();
            fs::write(unit.join("output"), output).unwrap();
            fs::write(
                unit.join("root-output"),
                unit.join("out").to_string_lossy().as_bytes(),
            )
            .unwrap();
            let fingerprint = source
                .target_dir
                .join("debug/.fingerprint")
                .join(name)
                .join(format!("run-build-script-{name}"));
            fs::create_dir_all(fingerprint.parent().unwrap()).unwrap();
            fs::write(&fingerprint, "run output").unwrap();
        }
        grove.promote(&source).unwrap();
        drop(source);

        let seeded = grove.seeded_tagged_lane("consumer").unwrap();

        let debug = seeded.target_dir.join("debug");
        assert!(debug.join("build/probe/output").exists());
        assert!(
            debug
                .join(".fingerprint/probe/run-build-script-probe")
                .exists()
        );
        assert!(!debug.join("build/leaky").exists());
        assert!(
            !debug
                .join(".fingerprint/leaky/run-build-script-leaky")
                .exists()
        );
    }

    #[test]
    fn repair_error_leaves_no_warm_lane_to_reuse() {
        let root = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let grove = Grove::with_root(root.path().to_path_buf(), workspace.path());
        let source = grove.tagged_lane("source").unwrap();
        let profile = source.target_dir.join("debug");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("build"), "not a build-script directory").unwrap();
        grove.promote(&source).unwrap();
        drop(source);
        let consumer = grove.tagged_lane("consumer").unwrap();
        let consumer_dir = consumer.dir.clone();
        drop(consumer);

        assert!(grove.seeded_tagged_lane("consumer").is_err());
        assert!(
            !consumer_dir.join("target").exists(),
            "a failed repair must not publish cloned target output"
        );
    }
}
