use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

use super::git::{
    Objects, Sparse, add_worktree, bytes, bytes_input, commit_tree, index_path, optional_text, run,
    temp_index, text, text_isolated,
};
use super::{Lease, State, capture, validate};

pub(super) const META: &str = ".grove-salvage";

pub(super) struct Archive {
    pub(super) reference: String,
    pub(super) original: State,
    pub(super) snapshot: String,
    pub(super) clean_index: Vec<u8>,
}

pub(super) struct Prepared {
    pub(super) archive: Archive,
    pub(super) state_commit: String,
}

pub(super) struct Located {
    pub(super) archive: Archive,
    pub(super) exact: bool,
}

pub(super) fn prepare(worktree: &Path, reference: String) -> Result<Prepared> {
    let original = capture(worktree)?;
    validate(worktree, &original.status)?;
    let index_path = index_path(worktree)?;
    let temp = temp_index(&index_path, &original.index)?;
    add_worktree(worktree, temp.path())?;
    let tree = text(worktree, &["write-tree"], Some(temp.path()))?;
    let message = format!("grove: salvage worktree state\n\nState: {reference}\n");
    let snapshot = commit_tree(worktree, &tree, &original.head, &message)?;
    let clean_index = fs::read(temp.path()).context("reading prepared clean index")?;
    add_metadata(worktree, temp.path(), &original, &snapshot, &clean_index)?;
    let state_tree = text(worktree, &["write-tree"], Some(temp.path()))?;
    let state_commit = commit_tree(
        worktree,
        &state_tree,
        &snapshot,
        "grove: preserve staged and working state\n",
    )?;
    Ok(Prepared {
        archive: Archive {
            reference,
            original: State { tree, ..original },
            snapshot,
            clean_index,
        },
        state_commit,
    })
}

fn add_metadata(
    worktree: &Path,
    index: &Path,
    state: &State,
    snapshot: &str,
    clean_index: &[u8],
) -> Result<()> {
    let head = format!("{}\n", state.head);
    let snapshot = format!("{snapshot}\n");
    let present = if state.sparse.patterns.is_some() {
        b"1\n"
    } else {
        b"0\n"
    };
    for (name, contents) in [
        ("version", b"1\n".as_slice()),
        ("head", head.as_bytes()),
        ("snapshot", snapshot.as_bytes()),
        ("index", state.index.as_slice()),
        ("clean-index", clean_index),
        ("status-v2", state.status.as_slice()),
        ("sparse-present", present.as_slice()),
        (
            "sparse-checkout",
            state.sparse.patterns.as_deref().unwrap_or_default(),
        ),
        ("sparse-config", state.sparse.config.as_slice()),
    ] {
        add_blob(worktree, index, name, contents)?;
    }
    Ok(())
}

fn add_blob(worktree: &Path, index: &Path, name: &str, contents: &[u8]) -> Result<()> {
    let object = String::from_utf8(bytes_input(
        worktree,
        &["hash-object", "-w", "--stdin"],
        None,
        contents,
    )?)?
    .trim()
    .to_string();
    let entry = format!("100644,{object},{META}/{name}");
    run(
        worktree,
        &["update-index", "--add", "--cacheinfo", &entry],
        Some(index),
        None,
    )
}

pub(super) fn locate(
    worktree: &Path,
    lease: &Lease,
    current_head: &str,
) -> Result<Option<Located>> {
    let exact = salvage_ref(lease, current_head);
    if let Some(archive) = load(worktree, &exact)? {
        return Ok(Some(Located {
            archive,
            exact: true,
        }));
    }
    let prefix = salvage_prefix(lease);
    let refs = text(
        worktree,
        &["for-each-ref", "--format=%(refname)", &prefix],
        None,
    )?;
    let mut found = None;
    for reference in refs.lines().filter(|reference| !reference.is_empty()) {
        let archive = load(worktree, reference)?
            .with_context(|| format!("salvage ref disappeared while reading {reference}"))?;
        if archive.snapshot != current_head {
            continue;
        }
        if found.is_some() {
            bail!("multiple salvage refs describe current HEAD; worktree left intact")
        }
        found = Some(Located {
            archive,
            exact: false,
        });
    }
    Ok(found)
}

pub(super) fn recovery_exists(worktree: &Path, lease: &Lease, current_head: &str) -> Result<bool> {
    let exact = salvage_ref(lease, current_head);
    if header(worktree, &exact)?.is_some() {
        return Ok(true);
    }
    let prefix = salvage_prefix(lease);
    let refs = text(
        worktree,
        &["for-each-ref", "--format=%(refname)", &prefix],
        None,
    )?;
    let mut found = false;
    for reference in refs.lines().filter(|reference| !reference.is_empty()) {
        let Some(snapshot) = header(worktree, reference)? else {
            bail!("salvage ref disappeared while reading {reference}")
        };
        if snapshot != current_head {
            continue;
        }
        if found {
            bail!("multiple salvage refs describe current HEAD; worktree left intact")
        }
        found = true;
    }
    Ok(found)
}

pub(super) fn exact_isolated(
    worktree: &Path,
    lease: &Lease,
    current_head: &str,
    objects: &Objects,
) -> Result<Option<Archive>> {
    load_with(worktree, &salvage_ref(lease, current_head), Some(objects))
}

fn header(worktree: &Path, reference: &str) -> Result<Option<String>> {
    if optional_text(worktree, &["rev-parse", "--verify", "--quiet", reference])?.is_none() {
        return Ok(None);
    }
    let version = blob(worktree, reference, "version")?;
    if version != b"1\n" {
        bail!("salvage ref {reference} has an unsupported evidence version")
    }
    let head = blob_text(worktree, reference, "head")?;
    let snapshot = blob_text(worktree, reference, "snapshot")?;
    text(worktree, &["rev-parse", &format!("{head}^{{tree}}")], None)?;
    text(
        worktree,
        &["rev-parse", &format!("{snapshot}^{{tree}}")],
        None,
    )?;
    validate_index(worktree, &blob(worktree, reference, "index")?)?;
    validate_index(worktree, &blob(worktree, reference, "clean-index")?)?;
    blob(worktree, reference, "status-v2")?;
    let present = blob(worktree, reference, "sparse-present")?;
    blob(worktree, reference, "sparse-checkout")?;
    blob(worktree, reference, "sparse-config")?;
    if !matches!(present.as_slice(), b"0\n" | b"1\n") {
        bail!("salvage ref {reference} has invalid sparse evidence")
    }
    Ok(Some(snapshot))
}

fn validate_index(worktree: &Path, contents: &[u8]) -> Result<()> {
    let index = index_path(worktree)?;
    let temp = temp_index(&index, contents)?;
    bytes(worktree, &["ls-files", "--stage", "-z"], Some(temp.path())).map(drop)
}

fn load(worktree: &Path, reference: &str) -> Result<Option<Archive>> {
    load_with(worktree, reference, None)
}

fn load_with(
    worktree: &Path,
    reference: &str,
    objects: Option<&Objects>,
) -> Result<Option<Archive>> {
    if header(worktree, reference)?.is_none() {
        return Ok(None);
    }
    let head = blob_text(worktree, reference, "head")?;
    let snapshot = blob_text(worktree, reference, "snapshot")?;
    let head_tree = text(worktree, &["rev-parse", &format!("{head}^{{tree}}")], None)?;
    let tree = text(
        worktree,
        &["rev-parse", &format!("{snapshot}^{{tree}}")],
        None,
    )?;
    let index = blob(worktree, reference, "index")?;
    let index_path = index_path(worktree)?;
    let temp = temp_index(&index_path, &index)?;
    let index_tree = match objects {
        Some(objects) => text_isolated(worktree, &["write-tree"], Some(temp.path()), objects)?,
        None => text(worktree, &["write-tree"], Some(temp.path()))?,
    };
    let present = blob(worktree, reference, "sparse-present")?;
    let patterns = match present.as_slice() {
        b"0\n" => None,
        b"1\n" => Some(blob(worktree, reference, "sparse-checkout")?),
        _ => bail!("salvage ref {reference} has invalid sparse evidence"),
    };
    Ok(Some(Archive {
        reference: reference.to_string(),
        snapshot,
        clean_index: blob(worktree, reference, "clean-index")?,
        original: State {
            head,
            head_tree,
            index,
            index_tree,
            status: blob(worktree, reference, "status-v2")?,
            sparse: Sparse {
                patterns,
                config: blob(worktree, reference, "sparse-config")?,
            },
            tree,
        },
    }))
}

fn blob(worktree: &Path, reference: &str, name: &str) -> Result<Vec<u8>> {
    bytes(
        worktree,
        &["show", &format!("{reference}:{META}/{name}")],
        None,
    )
}

fn blob_text(worktree: &Path, reference: &str, name: &str) -> Result<String> {
    Ok(String::from_utf8(blob(worktree, reference, name)?)?
        .trim()
        .to_string())
}

pub(super) fn create_ref(worktree: &Path, reference: &str, commit: &str) -> Result<()> {
    let head = text(worktree, &["rev-parse", "--verify", "HEAD"], None)?;
    let zero = "0".repeat(head.len());
    run(
        worktree,
        &["update-ref", reference, commit, &zero],
        None,
        None,
    )
    .with_context(|| format!("creating immutable salvage ref {reference}"))
}

fn salvage_prefix(lease: &Lease) -> String {
    let mut hash = Sha256::new();
    hash.update(lease.repo.as_bytes());
    hash.update([0]);
    hash.update(lease.workspace.as_bytes());
    hash.update([0]);
    hash.update(lease.branch.as_bytes());
    hash.update(lease.created_at.to_le_bytes());
    if !lease.generation.is_empty() {
        hash.update([0]);
        hash.update(lease.generation.as_bytes());
    }
    format!("refs/grove/salvage/{:x}", hash.finalize())
}

pub(super) fn salvage_ref(lease: &Lease, head: &str) -> String {
    format!("{}/{head}", salvage_prefix(lease))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salvage_refs_are_scoped_to_the_lease_generation() {
        let mut first: Lease = serde_json::from_value(serde_json::json!({
            "workspace": "/tmp/worktree", "branch": "grove/agent", "agent": "agent",
            "toolchain": "stable", "repo": "/tmp/repo/.git", "created_at": 1,
            "generation": "first", "base_oid": "abc"
        }))
        .unwrap();
        let mut second = first.clone();
        second.generation = "second".into();

        assert_ne!(salvage_ref(&first, "head"), salvage_ref(&second, "head"));
        first.generation = "second".into();
        assert_eq!(salvage_ref(&first, "head"), salvage_ref(&second, "head"));
    }
}
