//! Release-bundle staging, manifest construction, and artifact copying.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use super::{Artifact, Manifest, ReleaseReceipt, Report};
use crate::{cache, project, snapshot, verify};

#[allow(clippy::too_many_arguments)]
pub(super) fn stage_bundle(
    workspace: &Path,
    frozen_workspace: &Path,
    repo: &str,
    task_id: &str,
    profile: &str,
    requested: &[String],
    start: &snapshot::Snapshot,
    start_ref: snapshot::Ref,
    run: verify::VerifyReport,
    lane: &cache::Lane,
    stage: &File,
) -> Result<Report> {
    #[cfg(not(unix))]
    {
        let _ = (
            workspace,
            frozen_workspace,
            repo,
            task_id,
            profile,
            requested,
            start,
            start_ref,
            run,
            lane,
            stage,
        );
        bail!("secure frozen-release staging is not supported on this platform")
    }
    #[cfg(unix)]
    {
        if !run.passed || run.profile != profile {
            bail!("release verification did not produce a passing requested profile")
        }
        require_unchanged(workspace, start)?;
        require_unchanged(frozen_workspace, start)?;
        let lane_root = lane_root(&lane.dir)?;
        let mut seen = BTreeSet::new();
        let mut artifacts = Vec::new();
        for requested in requested {
            let relative = relative_artifact(requested)?;
            if !seen.insert(relative.clone()) {
                bail!("release artifact {relative:?} was specified more than once")
            }
            if relative == "manifest.json" {
                bail!("release artifact name {relative:?} is reserved")
            }
            let source = lane_source(&lane_root, Path::new(&relative), &relative)?;
            let (sha256, mode) = copy_with_hash(stage, Path::new(&relative), source)?;
            artifacts.push(Artifact {
                source: relative.clone(),
                path: relative,
                sha256,
                mode,
            });
        }
        require_unchanged(workspace, start)?;
        require_unchanged(frozen_workspace, start)?;
        let profile_sha256 = run
            .receipts
            .first()
            .map(|receipt| receipt.profile_sha256.clone())
            .context("release verification produced no receipts")?;
        if run.receipts.iter().any(|receipt| {
            receipt.profile != profile
                || receipt.profile_sha256 != profile_sha256
                || receipt.evidence.as_ref().is_none_or(|evidence| {
                    evidence.input != start_ref || evidence.output != start_ref
                })
        }) {
            bail!("release verification receipts do not prove the frozen snapshot")
        }
        let mut receipts = run.receipts;
        receipts.sort_by_key(|receipt| receipt.command_index);
        if receipts
            .iter()
            .map(|receipt| receipt.command_index)
            .collect::<BTreeSet<_>>()
            .len()
            != receipts.len()
        {
            bail!("release verification receipts repeat a command index")
        }
        let receipt_count = receipts.len();
        let toolchain = project::commands_toolchain(
            frozen_workspace,
            receipts.iter().map(|receipt| receipt.argv.as_slice()),
        );
        let commands = receipts
            .iter()
            .map(|receipt| receipt.argv.clone())
            .collect();
        let receipts = receipts
            .iter()
            .map(release_receipt)
            .collect::<Result<Vec<_>>>()?;
        let manifest = Manifest {
            schema_version: 3,
            grove_version: env!("CARGO_PKG_VERSION"),
            repository_id: cache::repo_slug(repo),
            task_id: task_id.into(),
            toolchain,
            profile: profile.into(),
            profile_sha256,
            commands,
            snapshot: start_ref.clone(),
            snapshot_manifest: start.clone(),
            receipts,
            verification: super::ReleaseVerification {
                passed: true,
                receipt_count,
            },
            artifacts: artifacts.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        let manifest_sha256 = hash(&bytes);
        write_file(stage, Path::new("manifest.json"), &bytes)?;
        require_unchanged(workspace, start)?;
        require_unchanged(frozen_workspace, start)?;
        Ok(Report {
            bundle: String::new(),
            manifest_sha256,
            snapshot: start_ref,
            artifacts,
        })
    }
}

fn release_receipt(receipt: &verify::Receipt) -> Result<ReleaseReceipt> {
    let evidence = receipt
        .evidence
        .as_ref()
        .context("release receipt lacks content evidence")?;
    Ok(ReleaseReceipt {
        profile: receipt.profile.clone(),
        run_id: receipt.run_id.clone(),
        profile_sha256: receipt.profile_sha256.clone(),
        command_index: receipt.command_index,
        required: receipt.required,
        checkout_head: evidence.checkout.head.clone(),
        changed_paths: evidence.checkout.changed_paths.clone(),
        input: evidence.input.clone(),
        output: evidence.output.clone(),
        lane_tag: receipt.lane.tag.clone(),
        argv: receipt.argv.clone(),
        started_at: receipt.started_at,
        ended_at: receipt.ended_at,
        duration_ms: receipt.duration_ms,
        exit_code: receipt.exit_code,
        interrupted: receipt.interrupted,
        test_count: receipt.test_count,
        passed: receipt.passed,
    })
}

fn require_unchanged(workspace: &Path, start: &snapshot::Snapshot) -> Result<()> {
    if snapshot::capture(workspace)?.sha256 != start.sha256 {
        bail!("workspace content drifted during frozen release")
    }
    Ok(())
}

fn relative_artifact(source: &str) -> Result<String> {
    let path = Path::new(source);
    if source.is_empty()
        || source.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("release artifact must be a relative lane path")
    }
    Ok(source.into())
}

#[cfg(unix)]
fn lane_root(lane: &Path) -> Result<File> {
    let expected = fs::symlink_metadata(lane).context("reading release lane")?;
    if !expected.is_dir() || expected.file_type().is_symlink() {
        bail!("release lane is not a real directory")
    }
    let root = File::open(lane).context("opening release lane")?;
    let actual = root.metadata().context("reading release lane")?;
    if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        bail!("release lane changed while opening it")
    }
    Ok(root)
}

#[cfg(unix)]
fn lane_source(lane: &File, relative: &Path, display: &str) -> Result<File> {
    use rustix::fs::{Mode, OFlags, openat};

    let parent = super::directory::parent(lane, relative, false, "release artifact")?;
    let name = relative
        .file_name()
        .context("release artifact has no file name")?;
    let source = File::from(
        openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("opening release artifact {display}"))?,
    );
    if !source.metadata()?.is_file() {
        bail!("release artifact {display:?} is not a regular file")
    }
    Ok(source)
}

#[cfg(unix)]
pub(super) fn copy_with_hash(
    stage: &File,
    relative: &Path,
    mut input: File,
) -> Result<(String, u32)> {
    use rustix::fs::{Mode, OFlags, openat};

    let metadata = input.metadata().context("reading release artifact")?;
    let parent = super::directory::parent(stage, relative, true, "release artifact")?;
    let name = relative
        .file_name()
        .context("release artifact has no file name")?;
    let mut output = File::from(
        openat(
            &parent,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .with_context(|| format!("creating release artifact {}", relative.display()))?,
    );
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
    }
    output.sync_all()?;
    output.set_permissions(metadata.permissions())?;
    Ok((crate::hex(&hash.finalize()), mode(&metadata)))
}

#[cfg(unix)]
pub(super) fn write_file(stage: &File, name: &Path, bytes: &[u8]) -> Result<()> {
    use rustix::fs::{Mode, OFlags, openat};

    if !matches!(name.components().next(), Some(Component::Normal(_)))
        || name.components().nth(1).is_some()
    {
        bail!("release metadata needs one normal file name")
    }
    let mut file = File::from(
        openat(
            stage,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .with_context(|| format!("creating release metadata {}", name.display()))?,
    );
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

fn hash(bytes: &[u8]) -> String {
    crate::hex(&Sha256::digest(bytes))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn held_stage_and_lane_fds_ignore_swapped_paths() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let output = dir.path().join("out");
        let victim = dir.path().join("victim");
        let lane = dir.path().join("lane");
        let held = dir.path().join("held-stage");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir(&output).unwrap();
        fs::create_dir(&victim).unwrap();
        fs::create_dir_all(lane.join("target/release")).unwrap();
        fs::write(victim.join("sentinel"), b"keep").unwrap();
        let destination =
            super::super::release_destination(&workspace, &output.join("bundle")).unwrap();
        let mut publication =
            super::super::publish::Publication::prepare(dir.path(), &workspace, destination)
                .unwrap();
        let stage = publication.stage_file().unwrap().try_clone().unwrap();
        let stage_path = publication.stage_path().to_path_buf();
        let artifact = lane.join("target/release/tool");
        fs::write(&artifact, b"good").unwrap();
        let root = lane_root(&lane).unwrap();
        let input = lane_source(&root, Path::new("target/release/tool"), "tool").unwrap();
        let replacement = lane.join("replacement");
        fs::write(&replacement, b"bad").unwrap();
        fs::rename(replacement, &artifact).unwrap();
        fs::rename(&stage_path, &held).unwrap();
        symlink(&victim, &stage_path).unwrap();

        assert_eq!(
            copy_with_hash(&stage, Path::new("target/release/tool"), input)
                .unwrap()
                .0,
            hash(b"good")
        );
        write_file(&stage, Path::new("manifest.json"), b"manifest").unwrap();
        assert_eq!(fs::read(held.join("target/release/tool")).unwrap(), b"good");
        assert_eq!(fs::read(held.join("manifest.json")).unwrap(), b"manifest");
        assert!(!victim.join("target/release/tool").exists());
        assert!(!victim.join("manifest.json").exists());
        assert!(publication.publish().is_err());
        drop(publication);
        assert_eq!(fs::read(victim.join("sentinel")).unwrap(), b"keep");
    }
}
