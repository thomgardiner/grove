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
/// back into the *source* lane and fails to build. Delete each build script's run
/// output and run fingerprint so Cargo reruns the already-compiled scripts in this
/// lane, regenerating correct paths. Compiled script binaries and crate rlibs stay,
/// so the copy-on-write win holds and only the cheap reruns repeat.
fn reset_seeded_build_scripts(lane: &Lane) {
    for base in [&lane.build_dir, &lane.target_dir] {
        let Ok(profiles) = fs::read_dir(base) else {
            continue;
        };
        for profile in profiles.flatten() {
            let profile = profile.path();
            // A build script's run output is the `build/<pkg>/` dir holding an
            // `output` file; its sibling holds the compiled binary, which is kept.
            if let Ok(units) = fs::read_dir(profile.join("build")) {
                for unit in units.flatten() {
                    if unit.path().join("output").exists() {
                        let _ = fs::remove_dir_all(unit.path());
                    }
                }
            }
            // Drop the matching run fingerprints so Cargo knows to rerun the scripts.
            if let Ok(prints) = fs::read_dir(profile.join(".fingerprint")) {
                for print in prints.flatten() {
                    let Ok(files) = fs::read_dir(print.path()) else {
                        continue;
                    };
                    for file in files.flatten() {
                        if file
                            .file_name()
                            .to_string_lossy()
                            .starts_with("run-build-script-")
                        {
                            let _ = fs::remove_file(file.path());
                        }
                    }
                }
            }
        }
    }
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
    if !super::retention::published_locked(root, canonical) {
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
    crate::seed::clone_tree_cow(canonical, &lane.dir, lane.require_cow)?;
    reset_seeded_build_scripts(lane);
    if let Some(meta) = meta {
        write_atomic(&lane.dir.join(".grove-meta.json"), &meta)?;
    }
    Ok(())
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
