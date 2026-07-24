//! Immutable candidate identity for verification, review, and finish binding.
//!
//! A candidate is not "current HEAD" and not "a clean worktree". It is the triple
//! `commit + tree + complete source digest` (ASSURANCE I1). Policy and task id are
//! provenance on the envelope, not part of the content-addressed candidate id.
//!
//! Capture materializes a dangling Git commit for the staged tree when the worktree
//! is dirty, anchors it under `refs/grove/candidates/<id>`, and never updates a
//! branch tip. Complete source identity is the Grove workspace snapshot digest
//! (tracked, staged, unstaged, deleted, and non-ignored untracked paths — the same
//! scope as verification snapshots). Gitignored paths are intentionally excluded.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{cache, git, project, snapshot, task};
use grove_core::task::Lifecycle;

pub const SCHEMA_VERSION: u32 = 1;

/// Machine-facing candidate object (ASSURANCE I1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub schema_version: u32,
    /// Content-addressed id of I1 fields only: base commit, tree, source digest.
    pub candidate_id: String,
    pub task_id: String,
    /// Checked-out commit when capture ran (branch tip / detached HEAD).
    pub base_commit: String,
    /// Immutable Git commit for the candidate tree. Equals `base_commit` when
    /// the worktree is fully clean; otherwise a commit of the index tree with
    /// parent `base_commit`, retained under `refs/grove/candidates/<id>`.
    pub candidate_commit: String,
    /// Tree object of `candidate_commit` (Git index tree at capture).
    pub candidate_tree: String,
    /// Complete workspace digest under Grove snapshot rules (see module docs).
    pub source_sha256: String,
    /// Task-pinned verification policy digest (I2 provenance; not in candidate_id).
    pub policy_sha256: String,
    pub captured_at: u64,
    /// True when porcelain is empty and the index tree matches `base_commit^{tree}`.
    pub clean: bool,
    /// True when `candidate_commit` was written as a Grove-retained object for a
    /// dirty index (not the branch tip).
    pub materialized: bool,
    /// True when every snapshot entry is represented by the Git index tree.
    /// False when non-ignored untracked paths or working-tree content diverge
    /// from the index; then `source_sha256` is the only complete identity.
    pub index_represents_source: bool,
    pub tracked: usize,
    pub untracked: usize,
    pub deleted: usize,
}

/// Capture and persist the task workspace as an immutable candidate object.
pub fn capture(root: &Path, workspace: &Path, task_id: &str) -> Result<Candidate> {
    let workspace = fs::canonicalize(workspace).context("canonicalizing workspace")?;
    let repo = project::repo_identity(&workspace);
    let loaded = task::load(root, &repo, task_id)?;
    if cache::canonical_path(Path::new(&loaded.workspace)) != workspace {
        bail!("task {task_id} belongs to another workspace")
    }
    if loaded.lifecycle != Lifecycle::Running && loaded.lifecycle != Lifecycle::Recovering {
        bail!("task {task_id} is not running")
    }
    let policy_sha256 = loaded
        .verification_policy_sha256
        .clone()
        .context("task has no pinned verification policy digest")?;

    let _lock = snapshot::workspace_lock(root, &workspace)?;
    let snap = snapshot::capture(&workspace)?;
    let reference = snapshot::persist(root, &repo, &snap)?;
    let base_commit = snap
        .head()
        .context("candidate capture requires a Git HEAD")?
        .to_string();
    let candidate_tree = snap
        .index_tree()
        .context("candidate capture requires a Git index tree")?
        .to_string();

    // Reject concurrent mutation before any flag or materialization decision.
    assert_frozen(&workspace, &snap.sha256, &base_commit)?;

    let flags = capture_flags(
        &workspace,
        &base_commit,
        &candidate_tree,
        reference.untracked,
    )?;
    let candidate_id = identity_id(&base_commit, &candidate_tree, &reference.sha256);

    let (candidate_commit, materialized) = if flags.clean {
        (base_commit.clone(), false)
    } else {
        (
            materialize_index_commit(&workspace, &base_commit, &candidate_tree)?,
            true,
        )
    };

    // Re-freeze and recompute flags so a change-and-restore between flag read and
    // materialization cannot record a transient clean/materialized state.
    assert_frozen(&workspace, &snap.sha256, &base_commit)?;
    let flags_again = capture_flags(
        &workspace,
        &base_commit,
        &candidate_tree,
        reference.untracked,
    )?;
    if flags_again != flags {
        bail!("workspace cleanliness flags changed while capturing candidate identity")
    }

    validate_commit_binding(
        &workspace,
        &candidate_commit,
        &candidate_tree,
        &base_commit,
        flags.clean,
    )?;

    let captured_at = now_secs();
    let candidate = Candidate {
        schema_version: SCHEMA_VERSION,
        candidate_id: candidate_id.clone(),
        task_id: task_id.to_string(),
        base_commit,
        candidate_commit: candidate_commit.clone(),
        candidate_tree,
        source_sha256: reference.sha256,
        policy_sha256,
        captured_at,
        clean: flags.clean,
        materialized,
        index_represents_source: flags.index_represents_source,
        tracked: reference.tracked,
        untracked: reference.untracked,
        deleted: reference.deleted,
    };
    // Retain every candidate commit (clean or materialized) so load remains
    // valid after branch tip moves, squash, or prune.
    retain_commit(
        &workspace,
        &candidate.candidate_id,
        &candidate.candidate_commit,
    )?;
    persist(root, &repo, &candidate)?;
    crate::events::record(
        root,
        &repo,
        "candidate.captured",
        serde_json::json!({
            "task_id": candidate.task_id,
            "candidate_id": candidate.candidate_id,
            "source_sha256": candidate.source_sha256,
            "clean": candidate.clean,
            "materialized": candidate.materialized,
            "index_represents_source": candidate.index_represents_source,
        }),
    );
    Ok(candidate)
}

/// Load a previously persisted candidate by content-addressed id.
pub fn load(root: &Path, workspace: &Path, candidate_id: &str) -> Result<Candidate> {
    let workspace = fs::canonicalize(workspace).context("canonicalizing workspace")?;
    let repo = project::repo_identity(&workspace);
    let path = candidate_path(root, &repo, candidate_id)?;
    let bytes = fs::read(&path).with_context(|| format!("no candidate {candidate_id}"))?;
    let candidate: Candidate =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if candidate.schema_version != SCHEMA_VERSION {
        bail!("unsupported candidate schema {}", candidate.schema_version)
    }
    if candidate.candidate_id != candidate_id {
        bail!("candidate id does not match persisted record")
    }
    let expected = identity_id(
        &candidate.base_commit,
        &candidate.candidate_tree,
        &candidate.source_sha256,
    );
    if candidate.candidate_id != expected {
        bail!("candidate identity fields do not rehash to candidate_id")
    }
    // Prove the complete-source snapshot still exists and rehashes.
    let snap = snapshot::validate(
        root,
        &repo,
        &snapshot::Ref {
            sha256: candidate.source_sha256.clone(),
            entries: candidate.tracked + candidate.untracked + candidate.deleted,
            tracked: candidate.tracked,
            untracked: candidate.untracked,
            deleted: candidate.deleted,
        },
    )?;
    if snap.sha256 != candidate.source_sha256 {
        bail!("candidate source snapshot digest mismatch")
    }
    validate_commit_binding(
        &workspace,
        &candidate.candidate_commit,
        &candidate.candidate_tree,
        &candidate.base_commit,
        candidate.clean,
    )?;
    let retained = git::capture(
        &workspace,
        &[
            "rev-parse",
            &format!("refs/grove/candidates/{}", candidate.candidate_id),
        ],
    )
    .context("candidate commit is not retained under refs/grove/candidates")?;
    if retained != candidate.candidate_commit {
        bail!("retained candidate ref does not match candidate_commit")
    }
    Ok(candidate)
}

fn persist(root: &Path, repo: &str, candidate: &Candidate) -> Result<()> {
    let path = candidate_path(root, repo, &candidate.candidate_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let existing: Candidate = serde_json::from_slice(&fs::read(&path)?)?;
        // I1 fields and derived flags must be identical; task/policy envelope may
        // update to the capturing task (rewritten below).
        if existing.candidate_id != candidate.candidate_id
            || existing.base_commit != candidate.base_commit
            || existing.candidate_tree != candidate.candidate_tree
            || existing.source_sha256 != candidate.source_sha256
            || existing.candidate_commit != candidate.candidate_commit
            || existing.clean != candidate.clean
            || existing.materialized != candidate.materialized
            || existing.index_represents_source != candidate.index_represents_source
        {
            bail!("candidate id collision with different identity fields")
        }
    }
    // Always rewrite so task_id / policy_sha256 / captured_at match this capture.
    crate::cache::write_atomic(&path, &serde_json::to_vec_pretty(candidate)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CaptureFlags {
    clean: bool,
    index_represents_source: bool,
}

fn capture_flags(
    workspace: &Path,
    base_commit: &str,
    candidate_tree: &str,
    untracked: usize,
) -> Result<CaptureFlags> {
    let base_tree = git::capture(
        workspace,
        &["rev-parse", &format!("{base_commit}^{{tree}}")],
    )?;
    let index_matches_base = candidate_tree == base_tree;
    let unstaged = git::capture(workspace, &["diff", "--name-only"])?;
    let index_represents_source = unstaged.is_empty() && untracked == 0;
    let porcelain = git::capture(workspace, &["status", "--porcelain"])?;
    let clean = porcelain.is_empty() && index_matches_base && index_represents_source;
    Ok(CaptureFlags {
        clean,
        index_represents_source,
    })
}

fn assert_frozen(workspace: &Path, source_sha256: &str, base_commit: &str) -> Result<()> {
    if snapshot::capture(workspace)?.sha256 != source_sha256 {
        bail!("workspace changed while capturing candidate identity")
    }
    if git::capture(workspace, &["rev-parse", "HEAD"])? != base_commit {
        bail!("HEAD moved while capturing candidate identity")
    }
    Ok(())
}

fn candidate_path(root: &Path, repo: &str, candidate_id: &str) -> Result<std::path::PathBuf> {
    if candidate_id.len() != 64 || !candidate_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid candidate id")
    }
    Ok(root
        .join("candidates")
        .join(cache::repo_slug(repo))
        .join(format!("{candidate_id}.json")))
}

fn identity_id(base_commit: &str, candidate_tree: &str, source_sha256: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"grove.candidate.v1\0");
    for part in [base_commit, candidate_tree, source_sha256] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    crate::hex(&hash.finalize())
}

/// Create a commit for the staged tree without moving any branch tip.
fn materialize_index_commit(workspace: &Path, parent: &str, tree: &str) -> Result<String> {
    let author_date = git::capture(workspace, &["log", "-1", "--format=%aI", parent])?;
    let committer_date = git::capture(workspace, &["log", "-1", "--format=%cI", parent])?;
    let mut command = Command::new("git");
    command
        .args(["commit-tree", tree, "-p", parent, "-m", "grove-candidate"])
        .current_dir(workspace)
        .env("GIT_AUTHOR_NAME", "grove")
        .env("GIT_AUTHOR_EMAIL", "grove@local")
        .env("GIT_AUTHOR_DATE", &author_date)
        .env("GIT_COMMITTER_NAME", "grove")
        .env("GIT_COMMITTER_EMAIL", "grove@local")
        .env("GIT_COMMITTER_DATE", &committer_date)
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = command
        .output()
        .context("spawning git commit-tree for candidate materialization")?;
    if !output.status.success() {
        bail!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !valid_oid(&oid) {
        bail!("git commit-tree returned an invalid object id")
    }
    let head = git::capture(workspace, &["rev-parse", "HEAD"])?;
    if head != parent {
        bail!("candidate materialization changed HEAD")
    }
    Ok(oid)
}

fn retain_commit(workspace: &Path, candidate_id: &str, commit: &str) -> Result<()> {
    let refname = format!("refs/grove/candidates/{candidate_id}");
    git::run(workspace, &["update-ref", &refname, commit])
        .with_context(|| format!("retaining candidate commit under {refname}"))?;
    Ok(())
}

fn validate_commit_binding(
    workspace: &Path,
    commit: &str,
    tree: &str,
    base: &str,
    clean: bool,
) -> Result<()> {
    if !valid_oid(commit) {
        bail!("candidate_commit is not a valid git object id")
    }
    let commit_tree = git::capture(workspace, &["rev-parse", &format!("{commit}^{{tree}}")])
        .context("resolving candidate_commit tree")?;
    if commit_tree != tree {
        bail!("candidate_commit tree does not match candidate_tree")
    }
    if clean {
        if commit != base {
            bail!("clean candidate_commit must equal base_commit")
        }
    } else {
        let parent = git::capture(workspace, &["rev-parse", &format!("{commit}^")])
            .context("resolving candidate_commit parent")?;
        if parent != base {
            bail!("materialized candidate_commit parent must equal base_commit")
        }
    }
    Ok(())
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_id_is_stable_for_same_fields() {
        let a = identity_id("c", "tree", "src");
        let b = identity_id("c", "tree", "src");
        let c = identity_id("c", "tree", "other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn identity_id_excludes_task_and_policy() {
        // Same I1 fields → same id regardless of who captured or which policy
        // was pinned (those are envelope fields on Candidate).
        assert_eq!(identity_id("c", "t", "s"), identity_id("c", "t", "s"));
    }
}
