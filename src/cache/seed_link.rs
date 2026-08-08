//! Hardlink seed profile trees into an isolated worktree lane.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

fn hardlink_file(from: &Path, to: &Path, force: bool) -> Result<bool> {
    if to.exists() {
        if !force {
            return Ok(false);
        }
        let _ = fs::remove_file(to);
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::hard_link(from, to) {
        Ok(()) => Ok(true),
        Err(_) => {
            fs::copy(from, to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
            Ok(true)
        }
    }
}

fn hardlink_tree(from: &Path, to: &Path, force: bool) -> Result<usize> {
    if !from.is_dir() {
        return Ok(0);
    }
    fs::create_dir_all(to)?;
    let mut linked = 0usize;
    for ent in fs::read_dir(from)? {
        let ent = ent?;
        let src = ent.path();
        let dst = to.join(ent.file_name());
        if ent.file_type()?.is_dir() {
            linked += hardlink_tree(&src, &dst, force)?;
        } else if ent.file_type()?.is_file() && hardlink_file(&src, &dst, force)? {
            linked += 1;
        }
    }
    Ok(linked)
}

pub(crate) fn seed_has_artifacts(seed: &Path) -> bool {
    seed.join("debug").join("deps").is_dir() || seed.join("release").join("deps").is_dir()
}

fn seed_fingerprint(workspace: &Path, seed: &Path) -> String {
    let mut h = Sha256::new();
    for rel in ["Cargo.lock", "Cargo.toml"] {
        let path = workspace.join(rel);
        if let Ok(meta) = fs::metadata(&path) {
            h.update(rel.as_bytes());
            if let Ok(modified) = meta.modified()
                && let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                h.update(since_epoch.as_secs().to_le_bytes());
                h.update(since_epoch.subsec_nanos().to_le_bytes());
            }
            h.update(meta.len().to_le_bytes());
        }
    }
    for profile in ["debug", "release"] {
        let deps = seed.join(profile).join("deps");
        if let Ok(rd) = fs::read_dir(&deps) {
            let mut names: Vec<_> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            h.update(profile.as_bytes());
            h.update((names.len() as u64).to_le_bytes());
            for name in names.iter().take(64) {
                h.update(name.as_bytes());
            }
            if names.len() > 64 {
                for name in names.iter().rev().take(64) {
                    h.update(name.as_bytes());
                }
            }
        }
    }
    crate::util::hex_lower(h.finalize())[..24].to_string()
}

/// Hardlink seed `deps` / `.fingerprint` / `build` / `incremental` when stamp is stale.
pub(crate) fn link_seed_into_lane(
    workspace: &Path,
    seed: &Path,
    lane_target: &Path,
) -> Result<usize> {
    if !seed_has_artifacts(seed) {
        return Ok(0);
    }
    let stamp = lane_target.join(".grove-seed-stamp");
    let fingerprint = seed_fingerprint(workspace, seed);
    if fs::read_to_string(&stamp)
        .map(|s| s.trim() == fingerprint)
        .unwrap_or(false)
    {
        return Ok(0);
    }
    fs::create_dir_all(lane_target)?;
    let mut linked = 0usize;
    for profile in ["debug", "release"] {
        let seed_profile = seed.join(profile);
        if !seed_profile.is_dir() {
            continue;
        }
        let lane_profile = lane_target.join(profile);
        for sub in ["deps", ".fingerprint", "build", "incremental"] {
            let from = seed_profile.join(sub);
            if from.is_dir() {
                linked += hardlink_tree(&from, &lane_profile.join(sub), true)?;
            }
        }
    }
    fs::write(stamp, format!("{fingerprint}\n"))?;
    Ok(linked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn hardlink_file_writes() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a");
        let to = dir.path().join("b");
        fs::write(&from, b"x").unwrap();
        assert!(hardlink_file(&from, &to, true).unwrap());
        assert_eq!(fs::read(&to).unwrap(), b"x");
    }
}
