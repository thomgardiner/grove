//! Release-bundle staging, manifest construction, and artifact copying.

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

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
    signer: &SigningKey,
    lane: &cache::Lane,
    stage: &Path,
) -> Result<Report> {
    if !run.passed || run.profile != profile {
        bail!("release verification did not produce a passing requested profile")
    }
    require_unchanged(workspace, start)?;
    require_unchanged(frozen_workspace, start)?;
    let mut seen = BTreeSet::new();
    let mut artifacts = Vec::new();
    for requested in requested {
        let relative = relative_artifact(requested)?;
        if !seen.insert(relative.clone()) {
            bail!("release artifact {relative:?} was specified more than once")
        }
        if matches!(relative.as_str(), "manifest.json" | "manifest.sig") {
            bail!("release artifact name {relative:?} is reserved")
        }
        let source = lane_source(&lane.dir, &relative)?;
        let destination = stage.join(&relative);
        let (sha256, mode) = copy_with_hash(&source, &destination)?;
        artifacts.push(Artifact {
            source: relative.clone(),
            path: relative,
            sha256,
            mode,
        });
    }
    require_unchanged(workspace, start)?;
    require_unchanged(frozen_workspace, start)?;
    let public = signer.verifying_key().to_bytes();
    let profile_sha256 = run
        .receipts
        .first()
        .map(|receipt| receipt.profile_sha256.clone())
        .context("release verification produced no receipts")?;
    if run.receipts.iter().any(|receipt| {
        receipt.profile != profile
            || receipt.profile_sha256 != profile_sha256
            || receipt
                .evidence
                .as_ref()
                .is_none_or(|evidence| evidence.input != start_ref || evidence.output != start_ref)
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
    let commands = receipts
        .iter()
        .map(|receipt| receipt.argv.clone())
        .collect();
    let receipts = receipts
        .iter()
        .map(release_receipt)
        .collect::<Result<Vec<_>>>()?;
    let manifest = Manifest {
        schema_version: 2,
        grove_version: env!("CARGO_PKG_VERSION"),
        repository_id: cache::repo_slug(repo),
        task_id: task_id.into(),
        toolchain: project::toolchain(frozen_workspace),
        profile: profile.into(),
        profile_sha256,
        commands,
        snapshot: start_ref.clone(),
        receipts,
        verification: super::ReleaseVerification {
            passed: true,
            receipt_count,
        },
        artifacts: artifacts.clone(),
        signer_public_key: STANDARD.encode(public),
        signer_key_id: hash(&public)[..16].into(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_sha256 = hash(&bytes);
    let signature = signer.sign(&bytes).to_bytes();
    write_file(&stage.join("manifest.json"), &bytes)?;
    write_file(
        &stage.join("manifest.sig"),
        format!("{}\n", STANDARD.encode(signature)).as_bytes(),
    )?;
    require_unchanged(workspace, start)?;
    require_unchanged(frozen_workspace, start)?;
    Ok(Report {
        bundle: stage.display().to_string(),
        manifest_sha256,
        snapshot: start_ref,
        artifacts,
    })
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
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("release artifact must be a relative lane path")
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn lane_source(lane: &Path, relative: &str) -> Result<PathBuf> {
    let source = lane.join(relative);
    let metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("reading release artifact {}", source.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("release artifact {relative:?} is not a regular file")
    }
    let lane = fs::canonicalize(lane).context("resolving release lane")?;
    let resolved = fs::canonicalize(&source).context("resolving release artifact")?;
    if !resolved.starts_with(&lane) {
        bail!("release artifact {relative:?} escapes its lane")
    }
    Ok(source)
}

fn copy_with_hash(source: &Path, destination: &Path) -> Result<(String, u32)> {
    let metadata = fs::metadata(source).with_context(|| format!("reading {}", source.display()))?;
    let parent = destination
        .parent()
        .context("release artifact has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut input = File::open(source).with_context(|| format!("opening {}", source.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
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
    drop(output);
    fs::set_permissions(destination, metadata.permissions())?;
    Ok((format!("{:x}", hash.finalize()), mode(&metadata)))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn mode(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        1
    } else {
        0
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
